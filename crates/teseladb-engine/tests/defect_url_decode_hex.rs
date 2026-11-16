#![cfg(target_os = "linux")]
//! A percent escape is `%` followed by two hexadecimal digits (RFC 3986, section 2.1).
//! Decoding input with a `%` followed by anything else fails, as input cut short after a
//! `%` does, instead of producing a byte computed from the non-hex characters.

use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::http::url::internal::{path_decode, url_decode};

#[test]
fn escapes_with_non_hex_digits_are_rejected() {
    for bad in [&b"%zz"[..], b"a%G1", b"%1g", b"%%41", b"x%-1y", b"%0 "] {
        let mut out = Sstring::from("unchanged");
        assert!(!url_decode(bad, &mut out), "url_decode({:?})", String::from_utf8_lossy(bad));
        assert_eq!(out, "unchanged");
        assert!(!path_decode(bad, &mut out), "path_decode({:?})", String::from_utf8_lossy(bad));
        assert_eq!(out, "unchanged");
    }
}

#[test]
fn escapes_with_hex_digits_still_decode() {
    let mut out = Sstring::new();
    assert!(url_decode(b"%2f%2F%41%7e+%c3%A9", &mut out));
    assert_eq!(out.as_bytes(), b"//A~ \xc3\xa9");
    let mut out = Sstring::new();
    assert!(path_decode(b"a+b%20c%09", &mut out));
    assert_eq!(out.as_bytes(), b"a+b c\t");
}
