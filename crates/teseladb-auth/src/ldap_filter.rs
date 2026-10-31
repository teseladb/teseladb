//! LDAP search filters as text (RFC 4515), written as the BER of a `Filter` (RFC 4511, section
//! 4.5.1), and the LDAP queries of RefDb's configuration (`userToDNMapping` and
//! `authz.queryTemplate`): `<base DN>?<attributes>?<scope>?<filter>`, the query part of an LDAP
//! URL (RFC 4516), percent-encoded.

use crate::ldap_ber::{self, BerError, MAX_DEPTH, tag};

fn bad(what: &'static str) -> BerError {
    BerError(what)
}

/// Escapes `value` for a filter (RFC 4515, section 3): `*`, `(`, `)`, `\` and NUL as `\XX`.
pub fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '*' | '(' | ')' | '\\' | '\0' => out.push_str(&format!("\\{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The bytes of an escaped assertion value (`\XX` pairs).
fn unescape(text: &str) -> Result<Vec<u8>, BerError> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            let hex = text.get(i + 1..i + 3).ok_or(bad("escape"))?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| bad("escape"))?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn is_attr(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b';')
}

/// The BER of filter `text`.
pub fn encode(text: &str) -> Result<Vec<u8>, BerError> {
    let (out, rest) = filter(text.trim(), 0)?;
    if !rest.is_empty() {
        return Err(bad("text after the filter"));
    }
    Ok(out)
}

/// One parenthesized filter at the start of `text`; its BER and the text after it.
fn filter(text: &str, depth: usize) -> Result<(Vec<u8>, &str), BerError> {
    if depth > MAX_DEPTH {
        return Err(bad("filter too deep"));
    }
    let inner = text.strip_prefix('(').ok_or(bad("filter without parentheses"))?;
    let (out, rest) = match inner.as_bytes().first() {
        Some(b'&') => list(0xa0, &inner[1..], depth)?,
        Some(b'|') => list(0xa1, &inner[1..], depth)?,
        Some(b'!') => {
            let (f, rest) = filter(&inner[1..], depth + 1)?;
            (ldap_ber::element(0xa2, &f), rest)
        }
        _ => {
            let end = inner.find(')').ok_or(bad("filter without its closing parenthesis"))?;
            (item(&inner[..end])?, &inner[end..])
        }
    };
    let rest = rest.strip_prefix(')').ok_or(bad("filter without its closing parenthesis"))?;
    Ok((out, rest))
}

fn list(tag_byte: u8, mut text: &str, depth: usize) -> Result<(Vec<u8>, &str), BerError> {
    let mut content = Vec::new();
    while text.starts_with('(') {
        let (f, rest) = filter(text, depth + 1)?;
        content.extend_from_slice(&f);
        text = rest;
    }
    if content.is_empty() {
        return Err(bad("empty filter list"));
    }
    Ok((ldap_ber::element(tag_byte, &content), text))
}

/// `attr op value` without parentheses.
fn item(text: &str) -> Result<Vec<u8>, BerError> {
    let eq = text.find('=').ok_or(bad("filter item without ="))?;
    let (lhs, value) = (&text[..eq], &text[eq + 1..]);
    let ava = |t: u8, attr: &str| -> Result<Vec<u8>, BerError> {
        if !is_attr(attr) {
            return Err(bad("attribute"));
        }
        let mut c = ldap_ber::element(tag::OCTET_STRING, attr.as_bytes());
        ldap_ber::put(&mut c, tag::OCTET_STRING, &unescape(value)?);
        Ok(ldap_ber::element(t, &c))
    };
    if let Some(attr) = lhs.strip_suffix('~') {
        return ava(0xa8, attr);
    }
    if let Some(attr) = lhs.strip_suffix('>') {
        return ava(0xa5, attr);
    }
    if let Some(attr) = lhs.strip_suffix('<') {
        return ava(0xa6, attr);
    }
    if let Some(spec) = lhs.strip_suffix(':') {
        return extensible(spec, value);
    }
    if !is_attr(lhs) {
        return Err(bad("attribute"));
    }
    if value == "*" {
        return Ok(ldap_ber::element(0x87, lhs.as_bytes()));
    }
    if !value.contains('*') {
        return ava(0xa3, lhs);
    }
    let parts: Vec<&str> = value.split('*').collect();
    let mut subs = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let t = if i == 0 {
            0x80
        } else if i == parts.len() - 1 {
            0x82
        } else {
            0x81
        };
        ldap_ber::put(&mut subs, t, &unescape(part)?);
    }
    let mut c = ldap_ber::element(tag::OCTET_STRING, lhs.as_bytes());
    ldap_ber::put(&mut c, tag::SEQUENCE, &subs);
    Ok(ldap_ber::element(0xa4, &c))
}

/// `[attr][:dn][:rule]:=value` (RFC 4515, section 3).
fn extensible(spec: &str, value: &str) -> Result<Vec<u8>, BerError> {
    let mut parts = spec.split(':');
    let attr = parts.next().unwrap_or_default();
    let (mut dn, mut rule) = (false, None);
    for p in parts {
        match p {
            "dn" if !dn && rule.is_none() => dn = true,
            r if rule.is_none() && is_attr(r) => rule = Some(r),
            _ => return Err(bad("extensible match")),
        }
    }
    if attr.is_empty() && rule.is_none() || !attr.is_empty() && !is_attr(attr) {
        return Err(bad("extensible match"));
    }
    let mut c = Vec::new();
    if let Some(r) = rule {
        ldap_ber::put(&mut c, 0x81, r.as_bytes());
    }
    if !attr.is_empty() {
        ldap_ber::put(&mut c, 0x82, attr.as_bytes());
    }
    ldap_ber::put(&mut c, 0x83, &unescape(value)?);
    if dn {
        ldap_ber::put(&mut c, 0x84, &[0xff]);
    }
    Ok(ldap_ber::element(0xa9, &c))
}

/// The scope of a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The base object alone.
    Base,
    /// Its children.
    One,
    /// The whole subtree.
    Sub,
}

impl Scope {
    /// Its value in a `SearchRequest`.
    pub fn code(self) -> i64 {
        match self {
            Scope::Base => 0,
            Scope::One => 1,
            Scope::Sub => 2,
        }
    }
}

/// A search from RefDb's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// Base DN.
    pub base: String,
    /// Attributes to return (none: every attribute).
    pub attributes: Vec<String>,
    /// Scope (`base` by default).
    pub scope: Scope,
    /// Filter text (`(objectClass=*)` by default).
    pub filter: String,
}

fn percent_decode(text: &str) -> Result<String, BerError> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = text.get(i + 1..i + 3).ok_or(bad("percent escape"))?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| bad("percent escape"))?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad("not UTF-8"))
}

impl Query {
    /// Reads `<base DN>?<attributes>?<scope>?<filter>`; missing parts take their defaults.
    pub fn parse(text: &str) -> Result<Query, BerError> {
        let parts: Vec<&str> = text.split('?').collect();
        if parts.len() > 4 {
            return Err(bad("query with more than four parts"));
        }
        let part = |i: usize| parts.get(i).copied().unwrap_or_default();
        let attributes = match part(1) {
            "" => Vec::new(),
            a => a.split(',').map(percent_decode).collect::<Result<_, _>>()?,
        };
        let scope = match part(2) {
            "" | "base" => Scope::Base,
            "one" => Scope::One,
            "sub" => Scope::Sub,
            _ => return Err(bad("scope")),
        };
        let filter = match percent_decode(part(3))? {
            f if f.is_empty() => "(objectClass=*)".to_owned(),
            f => f,
        };
        encode(&filter)?;
        Ok(Query { base: percent_decode(part(0))?, attributes, scope, filter })
    }
}
