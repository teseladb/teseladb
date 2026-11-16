//! URL percent-encoding and decoding.
//!
//! [`internal::url_decode`] decodes `%XX` escapes and `+` as a space (query strings);
//! [`internal::path_decode`] decodes `%XX` escapes only (path segments); both return
//! `false` when a `%` is not followed by two hexadecimal digits, or is cut short by the end
//! of the input. [`internal::url_encode`] escapes every byte outside the unreserved set.
//! Each call allocates its output at most once, sized up front.

/// The encoding and decoding functions.
pub mod internal {
    use crate::engine::core::sstring::{InitializedLater, Sstring};

    /// The value of the hexadecimal digit `c`, or `None` if it is not one.
    #[inline]
    fn hex_value(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    /// The byte of the two hexadecimal digits at `in_[from]` and `in_[from + 1]`, or `None`
    /// if either is not a hexadecimal digit.
    #[inline]
    fn hexstr_to_char(in_: &[u8], from: usize) -> Option<u8> {
        // SAFETY: the caller checked that from + 1 < in_.len().
        let (hi, lo) = unsafe { (*in_.get_unchecked(from), *in_.get_unchecked(from + 1)) };
        Some(hex_value(hi)? * 16 + hex_value(lo)?)
    }

    #[inline]
    fn should_encode(c: u8) -> bool {
        !(c.is_ascii_lowercase()
            || c.is_ascii_uppercase()
            || c.is_ascii_digit()
            || (c == b'-' || c == b'_' || c == b'.' || c == b'~'))
    }

    #[inline]
    fn char_to_hex(val: u8) -> u8 {
        // SAFETY: val is a nibble, < 16.
        unsafe { *b"0123456789ABCDEF".get_unchecked(val as usize) }
    }

    fn decode(replace_plus: bool, in_: &[u8], out: &mut Sstring) -> bool {
        let n = in_.len();
        let mut pos: usize = 0;
        let Ok(mut buff) = Sstring::initialized_later(InitializedLater, n) else {
            // Like `Sstring::from_bytes`, a length that does not fit an `Sstring` cannot
            // be recovered here.
            panic!("sstring overflow");
        };
        let dst = buff.data();
        let src = in_.as_ptr();
        let mut i: usize = 0;
        // SAFETY: in the whole loop, i < n reads inside `in_`; pos <= i < n writes inside `buff`,
        // which has n bytes, since every input byte produces at most one output byte.
        unsafe {
            while i < n {
                let c = *src.add(i);
                if c == b'%' {
                    if i + 3 > n {
                        return false;
                    }
                    let Some(b) = hexstr_to_char(in_, i + 1) else {
                        return false;
                    };
                    *dst.add(pos) = b;
                    pos += 1;
                    i += 2;
                } else if replace_plus && c == b'+' {
                    *dst.add(pos) = b' ';
                    pos += 1;
                } else {
                    *dst.add(pos) = c;
                    pos += 1;
                }
                i += 1;
            }
        }
        buff.resize(pos);
        *out = buff;
        true
    }

    /// Decodes the URL-encoded string `in_` (`%XX` escapes, and `+` as a space) into
    /// `out`; returns `false` if the input is malformed.
    pub fn url_decode(in_: &[u8], out: &mut Sstring) -> bool {
        decode(true, in_, out)
    }

    /// Decodes the URL path `in_` (`%XX` escapes; `+` is kept) into `out`; returns
    /// `false` if the input is malformed.
    pub fn path_decode(in_: &[u8], out: &mut Sstring) -> bool {
        decode(false, in_, out)
    }

    /// Url-encodes a string: everything but unreserved characters becomes `%XX`.
    pub fn url_encode(in_: &[u8]) -> Sstring {
        let mut encodable_chars: usize = 0;
        for &c in in_ {
            if should_encode(c) {
                encodable_chars += 1;
            }
        }
        if encodable_chars == 0 {
            return Sstring::from_bytes(in_);
        }
        let Ok(mut ret) =
            Sstring::initialized_later(InitializedLater, in_.len() + encodable_chars * 2)
        else {
            // A length that does not fit an `Sstring`, as in `decode`.
            panic!("sstring overflow");
        };
        let dst = ret.data();
        let mut o: usize = 0;
        // SAFETY: in the whole loop, `ret` has room for every input byte plus two per encoded byte,
        // which is exactly what the loop writes.
        unsafe {
            for &c in in_ {
                if should_encode(c) {
                    *dst.add(o) = b'%';
                    *dst.add(o + 1) = char_to_hex(c >> 4);
                    *dst.add(o + 2) = char_to_hex(c & 0xF);
                    o += 3;
                } else {
                    *dst.add(o) = c;
                    o += 1;
                }
            }
        }
        ret
    }
} // internal namespace
