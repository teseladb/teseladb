//! `encodeURI`, `encodeURIComponent`, `decodeURI`, `decodeURIComponent`, `escape` and `unescape`
//! (ECMA-262 §19.2.6, §B.2.1), over the UTF-16 units of the string.

use crate::builtins::{arg, method};
use crate::interp::{Interp, R};
use crate::strings::{from_units, units};
use crate::value::*;

/// The characters `encodeURI` keeps besides letters, digits and `-_.!~*'()`.
const URI_RESERVED: &str = ";/?:@&=+$,#";
const UNRESERVED_MARKS: &str = "-_.!~*'()";

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    for (name, f) in [
        ("encodeURI", encode_uri as NativeFn),
        ("encodeURIComponent", encode_uri_component),
        ("decodeURI", decode_uri),
        ("decodeURIComponent", decode_uri_component),
        ("escape", escape),
        ("unescape", unescape),
    ] {
        method(it, g, name, f, 1)?;
    }
    Ok(())
}

fn malformed<T>(it: &Interp) -> R<T> {
    Err(it.error("URIError", "malformed URI sequence"))
}

fn encode(it: &mut Interp, a: &[Val], keep: &str) -> R {
    let s = it.str_of(&arg(a, 0))?;
    let u = units(&s);
    let mut out = String::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        let c = u[i];
        if let Some(ch) = char::from_u32(u32::from(c)).filter(|ch| {
            ch.is_ascii_alphanumeric() || UNRESERVED_MARKS.contains(*ch) || keep.contains(*ch)
        }) {
            out.push(ch);
            i += 1;
            continue;
        }
        let cp = match c {
            0xDC00..=0xDFFF => return malformed(it),
            0xD800..=0xDBFF => match u.get(i + 1) {
                Some(&low @ 0xDC00..=0xDFFF) => {
                    i += 1;
                    0x10000 + ((u32::from(c) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
                }
                _ => return malformed(it),
            },
            _ => u32::from(c),
        };
        let mut buf = [0u8; 4];
        let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
        for b in ch.encode_utf8(&mut buf).bytes() {
            out.push_str(&format!("%{b:02X}"));
        }
        i += 1;
    }
    it.string(out)
}

fn encode_uri(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    encode(it, a, URI_RESERVED)
}

fn encode_uri_component(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    encode(it, a, "")
}

fn hex_byte(u: &[u16], i: usize) -> Option<u8> {
    if u.get(i) != Some(&u16::from(b'%')) {
        return None;
    }
    let d = |k: usize| u.get(i + k).and_then(|&c| char::from_u32(u32::from(c))?.to_digit(16));
    Some((d(1)? * 16 + d(2)?) as u8)
}

fn decode(it: &mut Interp, a: &[Val], keep: &str) -> R {
    let s = it.str_of(&arg(a, 0))?;
    let u = units(&s);
    let mut out: Vec<u16> = Vec::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        if u[i] != u16::from(b'%') {
            out.push(u[i]);
            i += 1;
            continue;
        }
        let Some(b) = hex_byte(&u, i) else { return malformed(it) };
        if b < 0x80 {
            let ch = char::from(b);
            if keep.contains(ch) {
                out.extend_from_slice(&u[i..i + 3]);
            } else {
                out.push(u16::from(b));
            }
            i += 3;
            continue;
        }
        let n = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return malformed(it),
        };
        let mut bytes = vec![b];
        for k in 1..n {
            match hex_byte(&u, i + 3 * k) {
                Some(c) if c & 0xC0 == 0x80 => bytes.push(c),
                _ => return malformed(it),
            }
        }
        let Ok(text) = std::str::from_utf8(&bytes) else { return malformed(it) };
        let mut buf = [0u16; 2];
        for ch in text.chars() {
            out.extend_from_slice(ch.encode_utf16(&mut buf));
        }
        i += 3 * n;
    }
    it.string(from_units(&out))
}

fn decode_uri(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    decode(it, a, URI_RESERVED)
}

fn decode_uri_component(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    decode(it, a, "")
}

/// `escape`: `%XX` below 256 and `%uXXXX` above, except letters, digits and `@*_+-./`.
fn escape(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = it.str_of(&arg(a, 0))?;
    let mut out = String::new();
    for c in units(&s) {
        match char::from_u32(u32::from(c)) {
            Some(ch) if ch.is_ascii_alphanumeric() || "@*_+-./".contains(ch) => out.push(ch),
            _ if c < 256 => out.push_str(&format!("%{c:02X}")),
            _ => out.push_str(&format!("%u{c:04X}")),
        }
    }
    it.string(out)
}

fn unescape(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = it.str_of(&arg(a, 0))?;
    let u = units(&s);
    let hex = |from: usize, n: usize| -> Option<u16> {
        let mut v = 0u16;
        for k in 0..n {
            let d = char::from_u32(u32::from(*u.get(from + k)?))?.to_digit(16)?;
            v = v * 16 + d as u16;
        }
        Some(v)
    };
    let mut out = Vec::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        if u[i] == u16::from(b'%') {
            if u.get(i + 1) == Some(&u16::from(b'u'))
                && let Some(v) = hex(i + 2, 4)
            {
                out.push(v);
                i += 6;
                continue;
            }
            if let Some(v) = hex(i + 1, 2) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(u[i]);
        i += 1;
    }
    it.string(from_units(&out))
}
