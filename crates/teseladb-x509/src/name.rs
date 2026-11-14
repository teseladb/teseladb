//! Distinguished names (RFC 5280, 4.1.2.4): parsing, comparison with the case-folding and
//! insignificant-space rules of RFC 4518 (as RFC 5280, 7.1 asks), the RFC 4514 string form and
//! construction.

use std::fmt::Write as _;

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
    #[must_use]
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
#[must_use]
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
            0x00..=0x1f | 0x7f..=0xff => {
                let _ = write!(out, "\\{b:02X}");
            }
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
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Name {
    /// Parses a DER `Name` (the whole SEQUENCE).
    ///
    /// # Errors
    ///
    /// [`Error::Der`] for a malformed name.
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
    /// Country names are `PrintableString`, as RFC 5280 requires; the rest `UTF8String`.
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
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The RDNs, most significant first.
    #[must_use]
    pub fn rdns(&self) -> &[Vec<Attribute>] {
        &self.rdns
    }

    /// Whether the name has no RDNs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rdns.is_empty()
    }

    /// Every value of attribute `oid`, as text.
    pub fn values(&self, attr: &[u8]) -> Vec<String> {
        self.rdns.iter().flatten().filter(|a| a.oid == attr).filter_map(Attribute::text).collect()
    }

    /// Whether `prefix` is an initial sequence of this name's RDNs (name constraints of type
    /// directoryName, RFC 5280, 4.2.1.10).
    #[must_use]
    pub fn starts_with(&self, prefix: &Name) -> bool {
        prefix.rdns.len() <= self.rdns.len()
            && prefix.rdns.iter().zip(&self.rdns).all(|(a, b)| rdn_eq(a, b))
    }

    /// The REFDB-X509 user name of a certificate with this subject: the RFC 2253 form that
    /// OpenSSL prints and RefDb uses, with every attribute in reverse order (also inside a
    /// multi-valued RDN), OpenSSL's short names and non-ASCII bytes escaped.
    #[must_use]
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
                    None => {
                        let _ = write!(out, "#{}", hex(&der::tlv(a.tag, &a.value)));
                    }
                }
            }
        }
        out
    }

    /// The RFC 4514 string: RDNs from the last to the first, joined with commas.
    #[must_use]
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

/// The value of an attribute as Go's `encoding/asn1` reads it into a string: `PrintableString` (with `*` and `&`, as Go
/// allows), `NumericString`, `IA5String`, `TeletexString` and `UTF8String` as their bytes, `BMPString` as UTF-16; `None` for any
/// other type.
fn go_text(a: &Attribute) -> Option<String> {
    const NUMERIC_STRING: u8 = 0x12;
    let v = &a.value;
    match a.tag {
        tag::PRINTABLE_STRING => v
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || b" '()+,-./:=?*&".contains(&c))
            .then(|| String::from_utf8_lossy(v).into_owned()),
        NUMERIC_STRING => v
            .iter()
            .all(|&c| c.is_ascii_digit() || c == b' ')
            .then(|| String::from_utf8_lossy(v).into_owned()),
        tag::IA5_STRING => v.is_ascii().then(|| String::from_utf8_lossy(v).into_owned()),
        tag::UTF8_STRING | tag::TELETEX_STRING => String::from_utf8(v.clone()).ok(),
        tag::BMP_STRING => decode_string(a.tag, v),
        _ => None,
    }
}

/// `value` escaped as Go's `pkix.RDNSequence.String` escapes it: `,+"\<>;` always, a space first or last and `#`
/// first, by byte position.
fn go_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (k, c) in value.char_indices() {
        let escape = match c {
            ',' | '+' | '"' | '\\' | '<' | '>' | ';' => true,
            ' ' => k == 0 || k == value.len() - 1,
            '#' => k == 0,
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The types Go's `pkix.Name` keeps in named fields, in the order its `ToRDNSequence` writes them, and their names.
const GO_FIELDS: [(&[u8], &str); 9] = [
    (oid::COUNTRY, "C"),
    (oid::STATE, "ST"),
    (oid::LOCALITY, "L"),
    (oid::STREET, "STREET"),
    (&[0x55, 0x04, 0x11], "POSTALCODE"),
    (oid::ORGANIZATION, "O"),
    (oid::ORGANIZATIONAL_UNIT, "OU"),
    (oid::COMMON_NAME, "CN"),
    (oid::SERIAL_NUMBER, "SERIALNUMBER"),
];

impl Name {
    /// The string Go's `crypto/x509/pkix.Name.String` writes for a parsed name, which a signature made by a Go program
    /// carries (the `X509IssuerName` of `XAdES`, for one), as Go 1.27 writes it. The attributes of other types go first,
    /// each in its own RDN, as `<dotted OID>=<the text>` when the value is a string and `<dotted OID>=#<hex of its DER>`
    /// otherwise; then the country, province, locality, street, postal
    /// code, organization and unit, each with all its values in one RDN, then the last common name and the last serial
    /// number; and the whole is written from the last RDN to the first.
    #[must_use]
    pub fn to_go_string(&self) -> String {
        let atvs: Vec<&Attribute> = self.rdns.iter().flatten().collect();
        let mut rdns: Vec<Vec<String>> = atvs
            .iter()
            .filter(|a| !GO_FIELDS.iter().any(|(o, _)| *o == a.oid.as_slice()))
            .map(|a| {
                let dotted = oid::to_dotted(&a.oid);
                vec![match go_text(a) {
                    Some(t) => format!("{dotted}={}", go_escape(&t)),
                    None => format!("{dotted}=#{}", hex(&der::tlv(a.tag, &a.value))),
                }]
            })
            .collect();
        for (i, (field, short)) in GO_FIELDS.iter().enumerate() {
            let values: Vec<String> = atvs
                .iter()
                .filter(|a| a.oid.as_slice() == *field)
                .filter_map(|a| go_text(a))
                .collect();
            let values = if i >= 7 { values.last().cloned().into_iter().collect() } else { values };
            if !values.is_empty() {
                rdns.push(values.iter().map(|v| format!("{short}={}", go_escape(v))).collect());
            }
        }
        rdns.iter().rev().map(|rdn| rdn.join("+")).collect::<Vec<_>>().join(",")
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
        assert_eq!(Name::from_der(a.der()).expect("the test input is valid"), a);
        assert!(Name::default().is_empty());
        assert_eq!(a.values(oid::COUNTRY), vec!["US".to_string()]);
    }

    /// The expected string is what Go 1.27.3 prints for the same attributes, in the same order, with
    /// `pkix.Name.FillFromRDNSequence` and `String`.
    #[test]
    fn go_strings_when_attributes_of_every_kind_follow_pkix_name_string_of_go_1_27() {
        let attr = |oid: &[u8], tag: u8, value: &[u8]| Attribute {
            oid: oid.to_vec(),
            tag,
            value: value.to_vec(),
        };
        let org_id = [0x55, 0x04, 0x61];
        let mut n = Name {
            der: Vec::new(),
            rdns: vec![
                vec![attr(&org_id, tag::PRINTABLE_STRING, b"VATES-Q2826004J")],
                vec![attr(oid::COMMON_NAME, tag::UTF8_STRING, b"first")],
                vec![attr(oid::COUNTRY, tag::PRINTABLE_STRING, b"ES")],
                vec![
                    attr(oid::ORGANIZATIONAL_UNIT, tag::TELETEX_STRING, b"b"),
                    attr(oid::ORGANIZATIONAL_UNIT, tag::IA5_STRING, b"a"),
                ],
                vec![attr(oid::COMMON_NAME, tag::BMP_STRING, &[0, b'#', 0, b'x', 0, b' '])],
                vec![attr(&[0x55, 0x04, 0x11], 0x12, b"28001")],
                vec![attr(&[0x2a, 0x03], tag::OCTET_STRING, &[1, 2])],
                vec![attr(oid::ORGANIZATION, tag::OCTET_STRING, &[1])],
                vec![attr(oid::LOCALITY, tag::UTF8_STRING, b"a,b+c;\"<>\\")],
                vec![attr(oid::STATE, tag::PRINTABLE_STRING, b"* & ok")],
                vec![attr(oid::STREET, 0x12, b"12")],
            ],
        };
        let go = r#"CN=\#x\ ,OU=b+OU=a,POSTALCODE=28001,STREET=12,L=a\,b\+c\;\"\<\>\\,ST=* & ok,C=ES,1.2.3=#04020102,2.5.4.97=VATES-Q2826004J"#;
        assert_eq!(n.to_go_string(), go);
        // Values Go does not parse are left as they are: a type of other kind with its DER, a named field not at all.
        n.rdns = vec![
            vec![attr(&[0x2a, 0x04], tag::PRINTABLE_STRING, b"bad!")],
            vec![attr(oid::SERIAL_NUMBER, tag::UTF8_STRING, &[0xff])],
            vec![attr(oid::STREET, 0x12, b"1a")],
        ];
        assert_eq!(n.to_go_string(), "1.2.4=#130462616421");
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
