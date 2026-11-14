//! PEM (RFC 7468) and the Base64 it uses (RFC 4648, section 4). Text outside the blocks is
//! ignored, as in the files that OpenSSL writes; inside a block only Base64 and whitespace are
//! accepted, after the RFC 1421 headers (`Proc-Type`, `DEK-Info`) that OpenSSL's legacy key
//! encryption writes.

use std::fmt::Write as _;

use crate::Error;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const LINE_LEN: usize = 64;

/// PEM labels.
pub mod label {
    /// An X.509 certificate.
    pub const CERTIFICATE: &str = "CERTIFICATE";
    /// An X.509 CRL.
    pub const CRL: &str = "X509 CRL";
    /// A PKCS #10 certification request.
    pub const CERTIFICATE_REQUEST: &str = "CERTIFICATE REQUEST";
    /// A PKCS #8 private key.
    pub const PRIVATE_KEY: &str = "PRIVATE KEY";
    /// A PKCS #8 encrypted private key.
    pub const ENCRYPTED_PRIVATE_KEY: &str = "ENCRYPTED PRIVATE KEY";
    /// A SEC 1 EC private key.
    pub const EC_PRIVATE_KEY: &str = "EC PRIVATE KEY";
    /// A PKCS #1 RSA private key.
    pub const RSA_PRIVATE_KEY: &str = "RSA PRIVATE KEY";
    /// A `SubjectPublicKeyInfo`.
    pub const PUBLIC_KEY: &str = "PUBLIC KEY";
}

/// One PEM block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The label of the `BEGIN` and `END` lines.
    pub label: String,
    /// The decoded contents.
    pub der: Vec<u8>,
    /// RFC 1421 headers before the contents, in order.
    pub headers: Vec<(String, String)>,
}

impl Block {
    /// The value of header `name`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // Blocks may hold private keys.
        self.der.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.der);
    }
}

/// The value of a Base64 character, classified without a branch or a table index that depends on
/// it (the technique of libsodium's decoder): PEM blocks may hold private keys, and a search of the
/// alphabet would take longer for some characters than for others.
fn sextet(c: u8) -> Option<u32> {
    let c = i32::from(c);
    // All ones when lo ≤ c ≤ hi, zero otherwise: both differences are negative only inside, and
    // they stay within ±256, so the shift by 8 leaves −1 or 0.
    let within = |lo: i32, hi: i32| ((lo - 1 - c) & (c - hi - 1)) >> 8;
    let (upper, lower, digit) = (within(65, 90), within(97, 122), within(48, 57));
    let (plus, slash) = (within(43, 43), within(47, 47));
    let value = (upper & (c - 65))
        | (lower & (c - 71))
        | (digit & (c + 4))
        | (plus & 0x3e)
        | (slash & 0x3f);
    let valid = upper | lower | digit | plus | slash;
    (valid != 0).then(|| u32::from(value.to_le_bytes()[0]))
}

/// Decodes padded Base64, ignoring ASCII whitespace (the line breaks of a PEM body).
///
/// # Errors
///
/// [`Error::Pem`] for a character outside Base64 or bad padding.
pub fn base64_decode(text: &[u8]) -> Result<Vec<u8>, Error> {
    let chars: Vec<u8> = text.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect();
    base64_decode_strict(&chars)
}

/// Decodes padded Base64 with nothing but its alphabet and padding, as SCRAM messages, SASL
/// payloads and tokens carry it (RFC 5802; the reference server refuses a space or a line break
/// inside).
///
/// # Errors
///
/// [`Error::Pem`] for a character outside Base64, bad padding or non-zero trailing bits.
pub fn base64_decode_strict(chars: &[u8]) -> Result<Vec<u8>, Error> {
    if !chars.len().is_multiple_of(4) {
        return Err(Error::Pem);
    }
    let mut out = Vec::with_capacity(chars.len() / 4 * 3);
    let quads = chars.chunks(4);
    let last = chars.len() / 4;
    for (i, quad) in quads.enumerate() {
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != last) {
            return Err(Error::Pem);
        }
        let mut acc = 0u32;
        for &c in &quad[..4 - pad] {
            acc = (acc << 6) | sextet(c).ok_or(Error::Pem)?;
        }
        for _ in 0..pad {
            acc <<= 6;
        }
        let bytes = acc.to_be_bytes();
        // Bits beyond the encoded bytes must be zero (canonical encoding).
        if (pad == 1 && bytes[3] != 0) || (pad == 2 && bytes[2] != 0) {
            return Err(Error::Pem);
        }
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Ok(out)
}

/// Encodes padded Base64.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut b = [0u8; 3];
        b[..chunk.len()].copy_from_slice(chunk);
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Every PEM block in `text`, in order.
///
/// # Errors
///
/// [`Error::Pem`] for a block without its end line, mismatched labels or bad Base64.
pub fn parse_all(text: &str) -> Result<Vec<Block>, Error> {
    let mut blocks = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(lbl) =
            line.trim_end().strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----"))
        else {
            continue;
        };
        let end = format!("-----END {lbl}-----");
        let mut body = String::new();
        let mut headers = Vec::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            let inner = inner.trim_end();
            if inner == end {
                closed = true;
                break;
            }
            if inner.starts_with("-----") {
                return Err(Error::Pem);
            }
            match inner.split_once(':') {
                // Headers only come before the contents.
                Some((name, value)) if body.is_empty() => {
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
                Some(_) => return Err(Error::Pem),
                None => body.push_str(inner),
            }
        }
        if !closed {
            return Err(Error::Pem);
        }
        let der = base64_decode(body.as_bytes())?;
        if der.is_empty() {
            return Err(Error::Pem);
        }
        blocks.push(Block { label: lbl.to_string(), der, headers });
    }
    Ok(blocks)
}

/// The contents of every block of `text` with label `wanted`.
///
/// # Errors
///
/// [`Error::Pem`] for malformed PEM; no block with the label `wanted` gives an empty list.
pub fn parse_label(text: &str, wanted: &str) -> Result<Vec<Vec<u8>>, Error> {
    Ok(parse_all(text)?.iter().filter(|b| b.label == wanted).map(|b| b.der.clone()).collect())
}

/// Writes one block with 64-character lines.
#[must_use]
pub fn encode(lbl: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let mut out = format!("-----BEGIN {lbl}-----\n");
    for chunk in b64.as_bytes().chunks(LINE_LEN) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    let _ = writeln!(out, "-----END {lbl}-----");
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_byte_classifies_as_the_alphabet_says() {
        for c in 0..=u8::MAX {
            let expected =
                ALPHABET.iter().position(|&a| a == c).map(|p| u32::try_from(p).expect("below 64"));
            assert_eq!(sextet(c), expected, "byte {c:#04x}");
        }
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, enc) in cases {
            assert_eq!(base64_encode(plain.as_bytes()), enc);
            assert_eq!(
                base64_decode(enc.as_bytes()).expect("the test input is valid"),
                plain.as_bytes()
            );
        }
        for bad in ["Zg=", "Z===", "Zh==", "Zm9=", "Zg==Zg==", "Zm9!", "Zm=v"] {
            assert_eq!(base64_decode(bad.as_bytes()), Err(Error::Pem), "{bad}");
        }
        assert_eq!(base64_decode(b"Zm9v\nYmFy").expect("the test input is valid"), b"foobar");
        assert_eq!(base64_decode_strict(b"Zm9vYmFy").expect("the test input is valid"), b"foobar");
        for bad in ["Zm9v\nYmFy", "Zm9v YmFy", "Zm9v\u{c}YmFy", " Zm9vYmFy"] {
            assert_eq!(base64_decode_strict(bad.as_bytes()), Err(Error::Pem), "{bad:?}");
        }
    }

    #[test]
    fn blocks_round_trip_and_reject_headers() {
        let der: Vec<u8> = (0..200u8).collect();
        let text =
            format!("junk\n{}more\n{}", encode("X", &der), encode(label::CERTIFICATE, &[1, 2]));
        let all = parse_all(&text).expect("the test input is valid");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].der, der);
        assert_eq!(
            parse_label(&text, label::CERTIFICATE).expect("the test input is valid"),
            vec![vec![1, 2]]
        );
        let headers =
            "-----BEGIN K-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: X,00\n\nAAAA\n-----END K-----\n";
        let b = parse_all(headers).expect("the test input is valid");
        assert_eq!(b[0].header("Proc-Type"), Some("4,ENCRYPTED"));
        assert_eq!(b[0].header("DEK-Info"), Some("X,00"));
        assert_eq!(b[0].header("Other"), None);
        let late = "-----BEGIN K-----\nAAAA\nProc-Type: 4,ENCRYPTED\n-----END K-----\n";
        assert_eq!(parse_all(late), Err(Error::Pem));
        assert_eq!(parse_all("-----BEGIN X-----\nAAAA\n"), Err(Error::Pem));
        assert_eq!(parse_all("-----BEGIN X-----\n-----END X-----\n"), Err(Error::Pem));
        assert_eq!(parse_all("-----BEGIN X-----\n-----BEGIN Y-----\n"), Err(Error::Pem));
    }
}
