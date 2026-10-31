//! `authenticationRestrictions` (FR-009): addresses in CIDR notation, and whether a client may
//! authenticate from its address to the node's. A user passes its own list when one of its
//! documents matches (or it has none), and must pass the list of every role it inherits; a
//! document matches when the client's address is in one of its `clientSource` blocks and the
//! node's in one of its `serverAddress` blocks, each only when given. A failure is
//! `Authentication failed.`, like any other.

use teseladb_bson::Value;

/// IPv4 in dotted decimal: its four bytes.
fn ipv4(text: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    let mut out = [0u8; 4];
    if parts.len() != 4 {
        return None;
    }
    for (o, p) in out.iter_mut().zip(&parts) {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *o = p.parse::<u8>().ok()?;
    }
    Some(out)
}

/// The 16-bit groups of one side of an IPv6 address around `::`.
fn groups(part: &str) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    if part.is_empty() {
        return Some(out);
    }
    let items: Vec<&str> = part.split(':').collect();
    for (i, g) in items.iter().enumerate() {
        if i + 1 == items.len() && g.contains('.') {
            let v4 = ipv4(g)?;
            out.push(u16::from_be_bytes([v4[0], v4[1]]));
            out.push(u16::from_be_bytes([v4[2], v4[3]]));
        } else if !g.is_empty() && g.len() <= 4 && g.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.push(u16::from_str_radix(g, 16).ok()?);
        } else {
            return None;
        }
    }
    Some(out)
}

/// IPv6 (RFC 4291, section 2.2): its sixteen bytes.
fn ipv6(text: &str) -> Option<[u8; 16]> {
    let all = match text.split_once("::") {
        Some((head, tail)) if !tail.contains("::") && !head.contains('.') => {
            let (h, t) = (groups(head)?, groups(tail)?);
            if h.len() + t.len() >= 8 {
                return None;
            }
            let mut all = h;
            all.resize(8 - t.len(), 0);
            all.extend(t);
            all
        }
        Some(_) => return None,
        None => groups(text).filter(|g| g.len() == 8)?,
    };
    let mut out = [0u8; 16];
    for (i, g) in all.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&g.to_be_bytes());
    }
    Some(out)
}

/// An address: 4 or 16 bytes.
pub fn address(text: &str) -> Option<Vec<u8>> {
    ipv4(text).map(|a| a.to_vec()).or_else(|| ipv6(text).map(|a| a.to_vec()))
}

/// A block in CIDR notation: the address and the bits of its prefix (all without `/`).
pub fn cidr(text: &str) -> Option<(Vec<u8>, u32)> {
    let (addr, prefix) = match text.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (text, None),
    };
    let bytes = address(addr)?;
    let max = bytes.len() as u32 * 8;
    let bits = match prefix {
        None => max,
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            p.parse::<u32>().ok().filter(|n| *n <= max)?
        }
        Some(_) => return None,
    };
    Some((bytes, bits))
}

/// Whether `addr` is in block `(net, bits)`.
fn contains(net: &[u8], bits: u32, addr: &[u8]) -> bool {
    if net.len() != addr.len() {
        return false;
    }
    (0..bits as usize).all(|i| {
        let mask = 0x80u8 >> (i % 8);
        net[i / 8] & mask == addr[i / 8] & mask
    })
}

/// The host part of `host:port`, `[v6]:port` or a bare address.
pub fn host(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match addr.rsplit_once(':') {
        Some((h, port)) if !h.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => addr,
    }
}

fn field_matches(doc: &teseladb_bson::Document, field: &str, addr: Option<&[u8]>) -> bool {
    let Some(Value::Array(blocks)) = doc.get(field) else { return true };
    let Some(addr) = addr else { return false };
    blocks.iter().any(|b| match b {
        Value::String(s) => cidr(s).is_some_and(|(net, bits)| contains(&net, bits, addr)),
        _ => false,
    })
}

/// Whether one of the restriction documents of `list` lets `client` reach `server` (an empty
/// list lets anyone).
pub fn list_allows(list: &[Value], client: Option<&str>, server: Option<&str>) -> bool {
    let client = client.and_then(|c| address(host(c)));
    let server = server.and_then(|s| address(host(s)));
    list.is_empty()
        || list.iter().any(|r| match r {
            Value::Document(d) => {
                field_matches(d, "clientSource", client.as_deref())
                    && field_matches(d, "serverAddress", server.as_deref())
            }
            _ => false,
        })
}

/// Whether a user with its own `restrictions` and the lists of the roles it inherits may
/// authenticate from `client` to `server`.
pub fn allows(
    own: Option<&[Value]>,
    roles: &[Value],
    client: Option<&str>,
    server: Option<&str>,
) -> bool {
    own.is_none_or(|l| list_allows(l, client, server))
        && roles.iter().all(|l| match l {
            Value::Array(l) => list_allows(l, client, server),
            _ => false,
        })
}
