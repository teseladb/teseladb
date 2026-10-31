//! Distinguished names (RFC 5280, 4.1.2.4): parsing, comparison with the case-folding and
//! insignificant-space rules of RFC 4518 (as RFC 5280, 7.1 asks), the RFC 4514 string form and
//! construction.

use crate::Error;
use crate::der::{self, tag};
use crate::oid;

/// One attribute of a relative distinguished name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// The attribute type, as OID contents.
    pub oid: Vec<u8>,
    /// The tag of the value.
    pub tag: u8,
    /// The contents of the value.
    pub value: Vec<u8>,
}

impl Attribute {
    /// The value as text, if it is one of the directory string types.
    pub fn text(&self) -> Option<String> {
        decode_string(self.tag, &self.value)
    }

    /// The comparison form: folded text for strings, the raw encoding otherwise.
    fn key(&self) -> Result<String, Vec<u8>> {
        match self.text() {
            Some(t) => Ok(fold(&t)),
            None => Err([&[self.tag][..], &self.value].concat()),
        }
    }
}

/// A distinguished name: a sequence of relative distinguished names.
#[derive(Clone, Debug)]
pub struct Name {
    der: Vec<u8>,
    rdns: Vec<Vec<Attribute>>,
}

/// Decodes the directory string types; `None` for any other tag or invalid contents.
pub fn decode_string(t: u8, v: &[u8]) -> Option<String> {
    match t {
        tag::UTF8_STRING => String::from_utf8(v.to_vec()).ok(),
        tag::PRINTABLE_STRING => v
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || b" '()+,-./:=?".contains(&c))
            .then(|| String::from_utf8_lossy(v).into_owned()),
        tag::IA5_STRING | tag::VISIBLE_STRING => {
            v.is_ascii().then(|| String::from_utf8_lossy(v).into_owned())
        }
        // TeletexString is read as Latin-1, as most implementations do.
        tag::TELETEX_STRING => Some(v.iter().map(|&c| char::from(c)).collect()),
        tag::BMP_STRING if v.len().is_multiple_of(2) => {
            let units: Vec<u16> = v.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16(&units).ok()
        }
        tag::UNIVERSAL_STRING if v.len().is_multiple_of(4) => v
            .chunks(4)
            .map(|c| char::from_u32(u32::from_be_bytes([c[0], c[1], c[2], c[3]])))
            .collect(),
        _ => None,
    }
}

/// RFC 4518 for case-ignore matching: case folded, leading and trailing spaces removed and
/// inner runs of spaces collapsed to one.
fn fold(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

impl Default for Name {
    /// The empty name (an empty SEQUENCE).
    fn default() -> Name {
        Name::build(&[])
    }
}

impl PartialEq for Name {
    /// RFC 5280, 7.1: same number of RDNs, each with the same set of attributes, values compared
    /// after RFC 4518 folding.
    fn eq(&self, other: &Name) -> bool {
        self.der == other.der
            || (self.rdns.len() == other.rdns.len()
                && self.rdns.iter().zip(&other.rdns).all(|(a, b)| rdn_eq(a, b)))
    }
}

impl Eq for Name {}

fn rdn_eq(a: &[Attribute], b: &[Attribute]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| x.oid == y.oid && x.key() == y.key()))
}

/// RFC 4514 short names of the attribute types.
const SHORT_NAMES: &[(&[u8], &str)] = &[
    (oid::COMMON_NAME, "CN"),
    (oid::LOCALITY, "L"),
    (oid::STATE, "ST"),
    (oid::ORGANIZATION, "O"),
    (oid::ORGANIZATIONAL_UNIT, "OU"),
    (oid::COUNTRY, "C"),
    (oid::STREET, "STREET"),
    (oid::DOMAIN_COMPONENT, "DC"),
    (oid::USER_ID, "UID"),
    (oid::EMAIL_ADDRESS, "emailAddress"),
    (oid::SERIAL_NUMBER, "serialNumber"),
];

/// OpenSSL's short names, as RefDb prints X.509 user names (measured with RefDb 9.0.4).
const OPENSSL_NAMES: &[(&[u8], &str)] = &[
    (oid::COMMON_NAME, "CN"),
    (&[0x55, 0x04, 0x04], "SN"),
    (oid::SERIAL_NUMBER, "serialNumber"),
    (oid::COUNTRY, "C"),
    (oid::LOCALITY, "L"),
    (oid::STATE, "ST"),
    (oid::STREET, "street"),
    (oid::ORGANIZATION, "O"),
    (oid::ORGANIZATIONAL_UNIT, "OU"),
    (&[0x55, 0x04, 0x0c], "title"),
    (&[0x55, 0x04, 0x0d], "description"),
    (&[0x55, 0x04, 0x0f], "businessCategory"),
    (&[0x55, 0x04, 0x11], "postalCode"),
    (&[0x55, 0x04, 0x29], "name"),
    (&[0x55, 0x04, 0x2a], "GN"),
    (&[0x55, 0x04, 0x2b], "initials"),
    (&[0x55, 0x04, 0x2c], "generationQualifier"),
    (&[0x55, 0x04, 0x2e], "dnQualifier"),
    (&[0x55, 0x04, 0x41], "pseudonym"),
    (oid::DOMAIN_COMPONENT, "DC"),
    (oid::USER_ID, "UID"),
    (oid::EMAIL_ADDRESS, "emailAddress"),
];

/// RFC 2253 escaping as OpenSSL writes it for RefDb: the special characters, a leading `#` or
/// space and a trailing space with a backslash, and control characters and every byte above 0x7F
/// of the UTF-8 form as
/// `\XX`.
fn escape_openssl(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        let first = i == 0;
        let last = i + 1 == bytes.len();
        match b {
            b',' | b'+' | b'"' | b'\\' | b'<' | b'>' | b';' => {
                out.push('\\');
                out.push(char::from(b));
            }
            b'#' if first => out.push_str("\\#"),
            b' ' if first || last => out.push_str("\\ "),
            0x00..=0x1f | 0x7f..=0xff => out.push_str(&format!("\\{b:02X}")),
            _ => out.push(char::from(b)),
        }
    }
    out
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let last = value.chars().count().saturating_sub(1);
    for (i, c) in value.chars().enumerate() {
        match c {
            ',' | '+' | '"' | '\\' | '<' | '>' | ';' => {
                out.push('\\');
                out.push(c);
            }
            '#' | ' ' if i == 0 => {
                out.push('\\');
                out.push(c);
            }
            ' ' if i == last => out.push_str("\\ "),
            '\0' => out.push_str("\\00"),
            _ => out.push(c),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Name {
    /// Parses a DER `Name` (the whole SEQUENCE).
    pub fn from_der(data: &[u8]) -> Result<Name, Error> {
        let tlv = der::read_single(data, tag::SEQUENCE)?;
        let mut rdns = Vec::new();
        let mut r = der::Reader::new(tlv.value);
        while !r.is_empty() {
            let set = r.read(tag::SET)?;
            let mut s = der::Reader::new(set);
            let mut rdn = Vec::new();
            while !s.is_empty() {
                let atv = s.read(tag::SEQUENCE)?;
                let mut a = der::Reader::new(atv);
                let oid = der::oid(a.read(tag::OID)?)?.to_vec();
                let v = a.read_any()?;
                a.finish()?;
                rdn.push(Attribute { oid, tag: v.tag, value: v.value.to_vec() });
            }
            if rdn.is_empty() {
                return Err(Error::Der);
            }
            rdns.push(rdn);
        }
        Ok(Name { der: tlv.raw.to_vec(), rdns })
    }

    /// Builds a name with one attribute per RDN, in the given order (most significant first).
    /// Country names are PrintableString, as RFC 5280 requires; the rest UTF8String.
    pub fn build(attrs: &[(&[u8], &str)]) -> Name {
        let rdns: Vec<Vec<Attribute>> = attrs
            .iter()
            .map(|(o, v)| {
                let t = if *o == oid::COUNTRY { tag::PRINTABLE_STRING } else { tag::UTF8_STRING };
                vec![Attribute { oid: o.to_vec(), tag: t, value: v.as_bytes().to_vec() }]
            })
            .collect();
        let encoded: Vec<Vec<u8>> = rdns
            .iter()
            .map(|rdn| {
                let atvs: Vec<Vec<u8>> = rdn
                    .iter()
                    .map(|a| der::sequence(&[&der::object_id(&a.oid), &der::tlv(a.tag, &a.value)]))
                    .collect();
                let parts: Vec<&[u8]> = atvs.iter().map(Vec::as_slice).collect();
                der::constructed(tag::SET, &parts)
            })
            .collect();
        let parts: Vec<&[u8]> = encoded.iter().map(Vec::as_slice).collect();
        Name { der: der::sequence(&parts), rdns }
    }

    /// The DER encoding.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The RDNs, most significant first.
    pub fn rdns(&self) -> &[Vec<Attribute>] {
        &self.rdns
    }

    /// Whether the name has no RDNs.
    pub fn is_empty(&self) -> bool {
        self.rdns.is_empty()
    }

    /// Every value of attribute `oid`, as text.
    pub fn values(&self, attr: &[u8]) -> Vec<String> {
        self.rdns.iter().flatten().filter(|a| a.oid == attr).filter_map(Attribute::text).collect()
    }

    /// Whether `prefix` is an initial sequence of this name's RDNs (name constraints of type
    /// directoryName, RFC 5280, 4.2.1.10).
    pub fn starts_with(&self, prefix: &Name) -> bool {
        prefix.rdns.len() <= self.rdns.len()
            && prefix.rdns.iter().zip(&self.rdns).all(|(a, b)| rdn_eq(a, b))
    }

    /// The REFDB-X509 user name of a certificate with this subject: the RFC 2253 form that
    /// OpenSSL prints and RefDb uses, with every attribute in reverse order (also inside a
    /// multi-valued RDN), OpenSSL's short names and non-ASCII bytes escaped.
    pub fn x509_user_name(&self) -> String {
        let mut out = String::new();
        for (i, rdn) in self.rdns.iter().rev().enumerate() {
            if i > 0 {
                out.push(',');
            }
            for (j, a) in rdn.iter().rev().enumerate() {
                if j > 0 {
                    out.push('+');
                }
                let typ = OPENSSL_NAMES
                    .iter()
                    .find(|(o, _)| *o == a.oid.as_slice())
                    .map_or_else(|| oid::to_dotted(&a.oid), |(_, s)| (*s).to_string());
                out.push_str(&typ);
                out.push('=');
                match a.text() {
                    Some(t) => out.push_str(&escape_openssl(&t)),
                    None => out.push_str(&format!("#{}", hex(&der::tlv(a.tag, &a.value)))),
                }
            }
        }
        out
    }

    /// The RFC 4514 string: RDNs from the last to the first, joined with commas.
    pub fn to_rfc4514(&self) -> String {
        let rdns: Vec<String> = self
            .rdns
            .iter()
            .rev()
            .map(|rdn| {
                let atvs: Vec<String> = rdn
                    .iter()
                    .map(|a| {
                        let typ = SHORT_NAMES
                            .iter()
                            .find(|(o, _)| *o == a.oid.as_slice())
                            .map_or_else(|| oid::to_dotted(&a.oid), |(_, s)| (*s).to_string());
                        match a.text() {
                            Some(t) => format!("{typ}={}", escape(&t)),
                            None => format!("{typ}=#{}", hex(&der::tlv(a.tag, &a.value))),
                        }
                    })
                    .collect();
                atvs.join("+")
            })
            .collect();
        rdns.join(",")
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn folding_matches_case_and_spaces_across_string_types() {
        let a = Name::build(&[(oid::COUNTRY, "US"), (oid::ORGANIZATION, "Test  Certificates ")]);
        let b = Name::build(&[(oid::COUNTRY, "us"), (oid::ORGANIZATION, " test certificates")]);
        assert_eq!(a, b);
        let c = Name::build(&[(oid::ORGANIZATION, "Test Certificates"), (oid::COUNTRY, "US")]);
        assert_ne!(a, c);
        assert!(a.starts_with(&Name::build(&[(oid::COUNTRY, "US")])));
        assert!(!a.starts_with(&c));
        assert!(!Name::build(&[(oid::COUNTRY, "US")]).starts_with(&a));
        let two = Name::build(&[(oid::COUNTRY, "US"), (oid::COUNTRY, "US")]);
        assert_ne!(a, Name::build(&[(oid::COUNTRY, "US"), (oid::ORGANIZATION, "x")]));
        assert_ne!(two, a);
        assert_eq!(Name::from_der(a.der()).unwrap(), a);
        assert!(Name::default().is_empty());
        assert_eq!(a.values(oid::COUNTRY), vec!["US".to_string()]);
    }

    #[test]
    fn rfc4514_strings() {
        let n = Name::build(&[
            (oid::COUNTRY, "US"),
            (oid::ORGANIZATION, "A, B+C"),
            (oid::COMMON_NAME, " x "),
        ]);
        assert_eq!(n.to_rfc4514(), "CN=\\ x\\ ,O=A\\, B\\+C,C=US");
        let mut odd = Name::build(&[(&[0x2a, 0x03], "#v\0;")]);
        assert_eq!(odd.to_rfc4514(), "1.2.3=\\#v\\00\\;");
        odd.rdns[0][0].tag = tag::OCTET_STRING;
        assert_eq!(odd.to_rfc4514(), "1.2.3=#0404237600 3b".replace(' ', ""));
        assert_eq!(odd.x509_user_name(), "1.2.3=#0404237600 3b".replace(' ', ""));
        let ctl = Name::build(&[(&[0x2a, 0x03], "a\u{1}b\u{7f}"), (oid::COMMON_NAME, "x")]);
        assert_eq!(ctl.x509_user_name(), "CN=x,1.2.3=a\\01b\\7F");
    }

    #[test]
    fn string_types() {
        assert_eq!(decode_string(tag::PRINTABLE_STRING, b"a*"), None);
        assert_eq!(decode_string(tag::IA5_STRING, &[0xc3]), None);
        assert_eq!(decode_string(tag::TELETEX_STRING, &[0xe9]).as_deref(), Some("é"));
        assert_eq!(decode_string(tag::BMP_STRING, &[0, 0x41]).as_deref(), Some("A"));
        assert_eq!(decode_string(tag::BMP_STRING, &[0]), None);
        assert_eq!(decode_string(tag::UNIVERSAL_STRING, &[0, 0, 0, 0x41]).as_deref(), Some("A"));
        assert_eq!(decode_string(tag::UNIVERSAL_STRING, &[0, 0x11, 0, 0]), None);
        assert_eq!(decode_string(tag::UNIVERSAL_STRING, &[0, 0, 0]), None);
        assert_eq!(decode_string(tag::UTF8_STRING, &[0xff]), None);
        assert_eq!(decode_string(tag::INTEGER, &[1]), None);
        // An empty RDN and trailing data inside an attribute are malformed.
        assert_eq!(Name::from_der(&[0x30, 0x02, 0x31, 0x00]).err(), Some(Error::Der));
        assert_eq!(
            Name::from_der(&[
                0x30, 0x0b, 0x31, 0x09, 0x30, 0x07, 0x06, 0x01, 0x2a, 0x05, 0x00, 0x05, 0x00
            ])
            .err(),
            Some(Error::Der)
        );
    }
}
