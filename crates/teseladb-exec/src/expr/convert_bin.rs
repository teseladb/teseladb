//! Binary data for `$convert` (measured on a `refrouter` 9.0.4): the text formats (`base64`,
//! `base64url`, `hex`, `utf8`, `uuid`) both ways and numbers as their bytes, little-endian unless
//! `byteOrder` is `big`.

/// The binary subtype of a UUID.
pub(super) const UUID_SUBTYPE: u8 = 4;
/// The bytes of a UUID.
const UUID_LEN: usize = 16;

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A text format of binary data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    Base64,
    Base64Url,
    Hex,
    Utf8,
    Uuid,
    Auto,
}

impl Format {
    /// The format named `name`.
    pub(super) fn of(name: &str) -> Option<Format> {
        Some(match name {
            "base64" => Format::Base64,
            "base64url" => Format::Base64Url,
            "hex" => Format::Hex,
            "utf8" => Format::Utf8,
            "uuid" => Format::Uuid,
            "auto" => Format::Auto,
            _ => return None,
        })
    }
}

/// Bytes in base64 (`url` for the URL alphabet, without padding).
pub(super) fn base64(bytes: &[u8], url: bool) -> String {
    let table = if url { URL } else { STANDARD };
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(table[((n >> (18 - 6 * i)) & 63) as usize]));
        }
        if !url {
            for _ in chunk.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

/// The bytes of base64 text, `None` when it is not valid.
pub(super) fn from_base64(text: &str, url: bool) -> Option<Vec<u8>> {
    let table = if url { URL } else { STANDARD };
    let body = text.trim_end_matches('=');
    if !url && !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in body.bytes() {
        let v = table.iter().position(|t| *t == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Bytes in uppercase hexadecimal.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// An ObjectId's text: its bytes in lowercase hexadecimal.
pub(super) fn object_id_text(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// The bytes of hexadecimal text, or RefDb's reason why not.
pub(super) fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("Hex blob with odd digit count".to_owned());
    }
    let digit = |c: u8| -> Result<u8, String> {
        char::from(c)
            .to_digit(16)
            .map(|d| d as u8)
            .ok_or_else(|| format!("The character \\x{c:02x} failed to parse from hex."))
    };
    text.as_bytes().chunks(2).map(|p| Ok(digit(p[0])? << 4 | digit(p[1])?)).collect()
}

/// A UUID's 16 bytes as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
pub(super) fn uuid_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() != UUID_LEN {
        return None;
    }
    let h = hex(bytes).to_lowercase();
    Some(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

/// The 16 bytes of UUID text `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
pub(super) fn from_uuid(text: &str) -> Option<Vec<u8>> {
    let groups: Vec<&str> = text.split('-').collect();
    let sizes = [8, 4, 4, 4, 12];
    if groups.len() != sizes.len() || groups.iter().zip(sizes).any(|(g, n)| g.len() != n) {
        return None;
    }
    from_hex(&groups.concat()).ok()
}

/// A number's bytes in the given order.
pub(super) fn number_bytes(le: Vec<u8>, big: bool) -> Vec<u8> {
    if big { le.into_iter().rev().collect() } else { le }
}

/// `BinData(0, "0102")`, as RefDb names a binary value in a message.
pub(super) fn shown(subtype: u8, bytes: &[u8]) -> String {
    format!("BinData({subtype}, \"{}\")", hex(bytes))
}
