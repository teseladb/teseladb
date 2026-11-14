//! Textual IP addresses (RFC 4291, 2.2, and the dotted-quad form of IPv4) to the bytes of an
//! iPAddress name. Written here because logic crates do not use `std::net`.

const V6_GROUPS: usize = 8;

/// IPv4 in dotted-decimal form, four decimal octets without leading zeros.
#[must_use]
pub fn parse_v4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in &mut out {
        let p = parts.next()?;
        if p.is_empty()
            || p.len() > 3
            || !p.bytes().all(|b| b.is_ascii_digit())
            || (p.len() > 1 && p.starts_with('0'))
        {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn groups(s: &str) -> Option<Vec<u16>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
    let pieces: Vec<&str> = s.split(':').collect();
    let mut out = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        if i + 1 == pieces.len() && p.contains('.') {
            let v4 = parse_v4(p)?;
            out.push(u16::from_be_bytes([v4[0], v4[1]]));
            out.push(u16::from_be_bytes([v4[2], v4[3]]));
        } else {
            if p.is_empty() || p.len() > 4 {
                return None;
            }
            out.push(u16::from_str_radix(p, 16).ok()?);
        }
    }
    Some(out)
}

/// IPv6 in the text forms of RFC 4291: eight groups, or fewer with one `::`, optionally ending
/// in a dotted IPv4 address.
#[must_use]
pub fn parse_v6(s: &str) -> Option<[u8; 16]> {
    let words = match s.split_once("::") {
        Some((head, tail)) => {
            if tail.contains("::") {
                return None;
            }
            let (h, t) = (groups(head)?, groups(tail)?);
            if h.len() + t.len() >= V6_GROUPS {
                return None;
            }
            let mut w = h;
            w.resize(V6_GROUPS - t.len(), 0);
            w.extend(t);
            w
        }
        None => groups(s).filter(|g| g.len() == V6_GROUPS)?,
    };
    let mut out = [0u8; 16];
    for (i, w) in words.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&w.to_be_bytes());
    }
    Some(out)
}

/// An IPv4 or IPv6 literal (IPv6 optionally in brackets) as 4 or 16 bytes.
#[must_use]
pub fn parse(s: &str) -> Option<Vec<u8>> {
    if let Some(v4) = parse_v4(s) {
        return Some(v4.to_vec());
    }
    let inner = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(s);
    parse_v6(inner).map(|v| v.to_vec())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn ipv4_and_ipv6_literals() {
        assert_eq!(parse("127.0.0.1"), Some(vec![127, 0, 0, 1]));
        for bad in
            ["1.2.3", "1.2.3.4.5", "1.2.3.256", "01.2.3.4", "1..3.4", "1.2.3.a", "1234.1.1.1"]
        {
            assert_eq!(parse_v4(bad), None, "{bad}");
        }
        let mut loopback = vec![0u8; 16];
        loopback[15] = 1;
        assert_eq!(parse("::1"), Some(loopback.clone()));
        assert_eq!(parse("[::1]"), Some(loopback));
        assert_eq!(parse("::"), Some(vec![0; 16]));
        assert_eq!(
            parse("2001:db8::ff00:42:8329"),
            Some(vec![0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0xff, 0, 0, 0x42, 0x83, 0x29])
        );
        assert_eq!(parse("1:2:3:4:5:6:7:8").map(|v| v[15]), Some(8));
        assert_eq!(parse("::ffff:10.0.0.1").map(|v| v[12..].to_vec()), Some(vec![10, 0, 0, 1]));
        assert_eq!(parse("1::").map(|v| v[1]), Some(1));
        for bad in [
            "1:2:3:4:5:6:7",
            "1::2::3",
            "1:2:3:4:5:6:7:8:9",
            "1:2:3:4::5:6:7:8",
            "12345::",
            "g::",
            ":1:2:3:4:5:6:7",
            "1::2:3.4.5",
            "host.example",
            "",
            "[::1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
