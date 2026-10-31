//! The BER of LDAP (RFC 4511, section 5.1; X.690): definite lengths only, written in their
//! shortest form and read in any form, since some servers write four-byte lengths for short
//! values. Elements nest no deeper than [`MAX_DEPTH`].

/// Deepest nesting read (a search filter of `and` inside `or`...).
pub const MAX_DEPTH: usize = 32;

/// Universal and LDAP tags this crate uses.
pub mod tag {
    /// BOOLEAN.
    pub const BOOLEAN: u8 = 0x01;
    /// INTEGER.
    pub const INTEGER: u8 = 0x02;
    /// OCTET STRING.
    pub const OCTET_STRING: u8 = 0x04;
    /// ENUMERATED.
    pub const ENUMERATED: u8 = 0x0a;
    /// SEQUENCE.
    pub const SEQUENCE: u8 = 0x30;
    /// SET.
    pub const SET: u8 = 0x31;
}

/// BER that cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BerError(pub &'static str);

impl std::fmt::Display for BerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LDAP message not valid: {}", self.0)
    }
}

impl std::error::Error for BerError {}

/// Appends element `tag` with `content`.
pub fn put(out: &mut Vec<u8>, tag: u8, content: &[u8]) {
    out.push(tag);
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
}

/// Element `tag` with `content`.
pub fn element(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    put(&mut out, tag, content);
    out
}

/// The content of an INTEGER (or ENUMERATED) of value `v`, in its shortest form.
pub fn int_content(v: i64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let mut i = 0;
    while i < 7
        && ((bytes[i] == 0 && bytes[i + 1] & 0x80 == 0)
            || (bytes[i] == 0xff && bytes[i + 1] & 0x80 != 0))
    {
        i += 1;
    }
    bytes[i..].to_vec()
}

/// An element: its tag, its content and the bytes after it.
pub type Element<'a> = (u8, &'a [u8], &'a [u8]);

/// Reads the element at the start of `input`; `Ok(None)` when `input` does not hold the whole
/// element yet.
pub fn read(input: &[u8]) -> Result<Option<Element<'_>>, BerError> {
    let Some(&tag) = input.first() else { return Ok(None) };
    if tag & 0x1f == 0x1f {
        return Err(BerError("multi-byte tag"));
    }
    let Some(&first) = input.get(1) else { return Ok(None) };
    let (len, head) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 {
            return Err(BerError("indefinite length"));
        }
        if n > 4 {
            return Err(BerError("length too large"));
        }
        let Some(bytes) = input.get(2..2 + n) else { return Ok(None) };
        (bytes.iter().fold(0usize, |acc, &b| acc << 8 | usize::from(b)), 2 + n)
    };
    match input.get(head..head + len) {
        Some(content) => Ok(Some((tag, content, &input[head + len..]))),
        None => Ok(None),
    }
}

/// Reads a whole element of `input` that must be there.
pub fn read_whole(input: &[u8]) -> Result<Element<'_>, BerError> {
    read(input)?.ok_or(BerError("truncated element"))
}

/// Reads an element that must have tag `want`; its content and the rest.
pub fn expect(input: &[u8], want: u8) -> Result<(&[u8], &[u8]), BerError> {
    match read_whole(input)? {
        (t, c, rest) if t == want => Ok((c, rest)),
        _ => Err(BerError("unexpected tag")),
    }
}

/// The value of INTEGER or ENUMERATED content.
pub fn int_value(content: &[u8]) -> Result<i64, BerError> {
    if content.is_empty() || content.len() > 8 {
        return Err(BerError("integer length"));
    }
    let negative = content[0] & 0x80 != 0;
    Ok(content.iter().fold(if negative { -1i64 } else { 0 }, |acc, &b| acc << 8 | i64::from(b)))
}

/// Every element of `content`, in order.
pub fn elements(mut content: &[u8]) -> Result<Vec<(u8, &[u8])>, BerError> {
    let mut out = Vec::new();
    while !content.is_empty() {
        let (t, c, rest) = read_whole(content)?;
        out.push((t, c));
        content = rest;
    }
    Ok(out)
}
