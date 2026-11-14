//! The extensions of RFC 5280 (4.2 and 5.2) that path validation and revocation use, and the
//! `GeneralName` type.

use crate::Error;
use crate::der::{self, tag};
use crate::name::Name;

/// One extension, undecoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
    /// The extension's OID contents.
    pub oid: Vec<u8>,
    /// Whether it is marked critical.
    pub critical: bool,
    /// The contents of `extnValue`.
    pub value: Vec<u8>,
}

impl Extension {
    /// The DER `Extension` SEQUENCE.
    #[must_use]
    pub fn to_der(&self) -> Vec<u8> {
        let oid = der::object_id(&self.oid);
        let value = der::octets(&self.value);
        if self.critical {
            der::sequence(&[&oid, &der::boolean_value(true), &value])
        } else {
            der::sequence(&[&oid, &value])
        }
    }
}

/// Reads `Extensions` (the SEQUENCE contents), rejecting duplicates and a DEFAULT FALSE written
/// out.
///
/// # Errors
///
/// [`Error::Der`] for malformed extensions, and [`Error::DuplicateExtension`] for one that appears
/// twice.
pub fn parse_extensions(data: &[u8]) -> Result<Vec<Extension>, Error> {
    let mut out: Vec<Extension> = Vec::new();
    let mut r = der::Reader::new(data);
    while !r.is_empty() {
        let mut e = der::Reader::new(r.read(tag::SEQUENCE)?);
        let oid = der::oid(e.read(tag::OID)?)?.to_vec();
        let critical = match e.read_optional(tag::BOOLEAN)? {
            Some(v) if der::boolean(v)? => true,
            Some(_) => return Err(Error::Der),
            None => false,
        };
        let value = e.read(tag::OCTET_STRING)?.to_vec();
        e.finish()?;
        if out.iter().any(|x| x.oid == oid) {
            return Err(Error::DuplicateExtension);
        }
        out.push(Extension { oid, critical, value });
    }
    Ok(out)
}

/// `keyUsage` bits (RFC 5280, 4.2.1.3), as positions in the mask.
pub mod key_usage {
    /// digitalSignature.
    pub const DIGITAL_SIGNATURE: u16 = 1 << 0;
    /// nonRepudiation (contentCommitment).
    pub const NON_REPUDIATION: u16 = 1 << 1;
    /// keyEncipherment.
    pub const KEY_ENCIPHERMENT: u16 = 1 << 2;
    /// dataEncipherment.
    pub const DATA_ENCIPHERMENT: u16 = 1 << 3;
    /// keyAgreement.
    pub const KEY_AGREEMENT: u16 = 1 << 4;
    /// keyCertSign.
    pub const KEY_CERT_SIGN: u16 = 1 << 5;
    /// cRLSign.
    pub const CRL_SIGN: u16 = 1 << 6;
}

/// `ReasonFlags` bits (RFC 5280, 4.2.1.13), as positions in the mask.
pub mod reasons {
    /// keyCompromise.
    pub const KEY_COMPROMISE: u16 = 1 << 1;
    /// All the reasons a CRL can cover (bits 1 to 8).
    pub const ALL: u16 = 0x1fe;
}

/// A `GeneralName`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeneralName {
    /// rfc822Name.
    Email(String),
    /// dNSName.
    Dns(String),
    /// directoryName.
    Directory(Name),
    /// uniformResourceIdentifier.
    Uri(String),
    /// iPAddress: 4 or 16 bytes, or 8 or 32 with the mask in name constraints.
    Ip(Vec<u8>),
    /// Any other form (otherName, x400Address, ediPartyName, registeredID): tag and contents.
    Other(u8, Vec<u8>),
}

fn ia5(v: &[u8]) -> Result<String, Error> {
    if v.is_ascii() { Ok(String::from_utf8_lossy(v).into_owned()) } else { Err(Error::Der) }
}

impl GeneralName {
    /// Reads one `GeneralName`.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when the input is not that DER structure.
    pub fn read(r: &mut der::Reader<'_>) -> Result<GeneralName, Error> {
        let t = r.read_any()?;
        Ok(match t.tag {
            0x81 => GeneralName::Email(ia5(t.value)?),
            0x82 => GeneralName::Dns(ia5(t.value)?),
            0xa4 => GeneralName::Directory(Name::from_der(
                der::read_single(t.value, tag::SEQUENCE)?.raw,
            )?),
            0x86 => GeneralName::Uri(ia5(t.value)?),
            0x87 => GeneralName::Ip(t.value.to_vec()),
            0xa0 | 0xa3 | 0xa5 | 0x88 => GeneralName::Other(t.tag, t.value.to_vec()),
            _ => return Err(Error::Der),
        })
    }

    /// The DER encoding.
    #[must_use]
    pub fn to_der(&self) -> Vec<u8> {
        match self {
            GeneralName::Email(s) => der::tlv(0x81, s.as_bytes()),
            GeneralName::Dns(s) => der::tlv(0x82, s.as_bytes()),
            GeneralName::Directory(n) => der::tlv(0xa4, n.der()),
            GeneralName::Uri(s) => der::tlv(0x86, s.as_bytes()),
            GeneralName::Ip(b) => der::tlv(0x87, b),
            GeneralName::Other(t, v) => der::tlv(*t, v),
        }
    }
}

/// Reads `GeneralNames`, which must not be empty.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_general_names(data: &[u8]) -> Result<Vec<GeneralName>, Error> {
    let mut r = der::Reader::new(data);
    let mut out = Vec::new();
    while !r.is_empty() {
        out.push(GeneralName::read(&mut r)?);
    }
    if out.is_empty() {
        return Err(Error::Der);
    }
    Ok(out)
}

/// `basicConstraints`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BasicConstraints {
    /// `cA`.
    pub ca: bool,
    /// `pathLenConstraint`.
    pub path_len: Option<u64>,
}

/// Parses `basicConstraints`.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_basic_constraints(v: &[u8]) -> Result<BasicConstraints, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let ca = match r.read_optional(tag::BOOLEAN)? {
        Some(b) if der::boolean(b)? => true,
        Some(_) => return Err(Error::Der),
        None => false,
    };
    let path_len = r.read_optional(tag::INTEGER)?.map(der::small_unsigned).transpose()?;
    r.finish()?;
    Ok(BasicConstraints { ca, path_len })
}

/// `nameConstraints`: permitted and excluded subtrees (only `base`; RFC 5280 requires minimum 0
/// and no maximum).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameConstraints {
    /// Permitted subtrees, if the field is present.
    pub permitted: Option<Vec<GeneralName>>,
    /// Excluded subtrees.
    pub excluded: Vec<GeneralName>,
}

fn parse_subtrees(v: &[u8]) -> Result<Vec<GeneralName>, Error> {
    let mut r = der::Reader::new(v);
    let mut out = Vec::new();
    while !r.is_empty() {
        let mut s = der::Reader::new(r.read(tag::SEQUENCE)?);
        out.push(GeneralName::read(&mut s)?);
        if let Some(min) = s.read_optional(tag::context_primitive(0))?
            && der::small_unsigned(min)? != 0
        {
            return Err(Error::UnknownCriticalExtension);
        }
        if s.read_optional(tag::context_primitive(1))?.is_some() {
            return Err(Error::UnknownCriticalExtension);
        }
        s.finish()?;
    }
    if out.is_empty() {
        return Err(Error::Der);
    }
    Ok(out)
}

/// Parses `nameConstraints`.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_name_constraints(v: &[u8]) -> Result<NameConstraints, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let permitted = r.read_optional(tag::context(0))?.map(parse_subtrees).transpose()?;
    let excluded =
        r.read_optional(tag::context(1))?.map(parse_subtrees).transpose()?.unwrap_or_default();
    r.finish()?;
    if permitted.is_none() && excluded.is_empty() {
        return Err(Error::Der);
    }
    Ok(NameConstraints { permitted, excluded })
}

/// Parses `certificatePolicies` into the policy OIDs (qualifiers are not processed).
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_policies(v: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let mut out: Vec<Vec<u8>> = Vec::new();
    while !r.is_empty() {
        let mut p = der::Reader::new(r.read(tag::SEQUENCE)?);
        let o = der::oid(p.read(tag::OID)?)?.to_vec();
        p.read_optional(tag::SEQUENCE)?;
        p.finish()?;
        if out.contains(&o) {
            return Err(Error::Der);
        }
        out.push(o);
    }
    if out.is_empty() {
        return Err(Error::Der);
    }
    Ok(out)
}

/// One `policyMappings` pair: (issuerDomainPolicy, subjectDomainPolicy), both OIDs.
pub type PolicyMapping = (Vec<u8>, Vec<u8>);

/// Parses `policyMappings` into (issuerDomainPolicy, subjectDomainPolicy) pairs.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_policy_mappings(value: &[u8]) -> Result<Vec<PolicyMapping>, Error> {
    let mut list = der::Reader::new(der::read_single(value, tag::SEQUENCE)?.value);
    let mut out = Vec::new();
    while !list.is_empty() {
        let mut mapping = der::Reader::new(list.read(tag::SEQUENCE)?);
        let issuer_policy = der::oid(mapping.read(tag::OID)?)?.to_vec();
        let subject_policy = der::oid(mapping.read(tag::OID)?)?.to_vec();
        mapping.finish()?;
        out.push((issuer_policy, subject_policy));
    }
    if out.is_empty() {
        return Err(Error::Der);
    }
    Ok(out)
}

/// `policyConstraints`: `requireExplicitPolicy` and `inhibitPolicyMapping`.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_policy_constraints(v: &[u8]) -> Result<(Option<u64>, Option<u64>), Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let require =
        r.read_optional(tag::context_primitive(0))?.map(der::small_unsigned).transpose()?;
    let inhibit =
        r.read_optional(tag::context_primitive(1))?.map(der::small_unsigned).transpose()?;
    r.finish()?;
    if require.is_none() && inhibit.is_none() {
        return Err(Error::Der);
    }
    Ok((require, inhibit))
}

/// The name of a distribution point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DistributionPointName {
    /// fullName.
    Full(Vec<GeneralName>),
    /// nameRelativeToCRLIssuer: the contents of one RDN, appended to the CRL issuer's name.
    Relative(Vec<u8>),
}

impl DistributionPointName {
    /// The names it stands for: a relative name is appended to `issuer`.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when the relative name is malformed.
    pub fn resolve(&self, issuer: &Name) -> Result<Vec<GeneralName>, Error> {
        match self {
            DistributionPointName::Full(names) => Ok(names.clone()),
            DistributionPointName::Relative(rdn) => {
                let base = der::read_single(issuer.der(), tag::SEQUENCE)?.value;
                let full = der::sequence(&[base, &der::tlv(tag::SET, rdn)]);
                Ok(vec![GeneralName::Directory(Name::from_der(&full)?)])
            }
        }
    }
}

/// One `DistributionPoint` of `cRLDistributionPoints`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DistributionPoint {
    /// distributionPoint.
    pub name: Option<DistributionPointName>,
    /// reasons, if present.
    pub reasons: Option<u16>,
    /// cRLIssuer.
    pub crl_issuer: Vec<GeneralName>,
}

fn read_dp_name(v: &[u8]) -> Result<DistributionPointName, Error> {
    let mut r = der::Reader::new(v);
    let t = r.read_any()?;
    r.finish()?;
    match t.tag {
        0xa0 => Ok(DistributionPointName::Full(parse_general_names(t.value)?)),
        0xa1 if !t.value.is_empty() => Ok(DistributionPointName::Relative(t.value.to_vec())),
        _ => Err(Error::Der),
    }
}

/// Parses `cRLDistributionPoints`.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_distribution_points(v: &[u8]) -> Result<Vec<DistributionPoint>, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let mut out = Vec::new();
    while !r.is_empty() {
        let mut d = der::Reader::new(r.read(tag::SEQUENCE)?);
        let name = d.read_optional(tag::context(0))?.map(read_dp_name).transpose()?;
        let reasons =
            d.read_optional(tag::context_primitive(1))?.map(der::named_bits_mask).transpose()?;
        let crl_issuer = d
            .read_optional(tag::context(2))?
            .map(parse_general_names)
            .transpose()?
            .unwrap_or_default();
        d.finish()?;
        if name.is_none() && crl_issuer.is_empty() {
            return Err(Error::Der);
        }
        out.push(DistributionPoint { name, reasons, crl_issuer });
    }
    if out.is_empty() {
        return Err(Error::Der);
    }
    Ok(out)
}

/// `issuingDistributionPoint` of a CRL.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the fields are the independent BOOLEANs of RFC 5280, section 5.2.5"
)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssuingDistributionPoint {
    /// distributionPoint.
    pub name: Option<DistributionPointName>,
    /// onlyContainsUserCerts.
    pub only_user: bool,
    /// onlyContainsCACerts.
    pub only_ca: bool,
    /// onlySomeReasons.
    pub only_some_reasons: Option<u16>,
    /// indirectCRL.
    pub indirect: bool,
    /// onlyContainsAttributeCerts.
    pub only_attribute: bool,
}

fn flag(r: &mut der::Reader<'_>, n: u8) -> Result<bool, Error> {
    match r.read_optional(tag::context_primitive(n))? {
        Some(v) if der::boolean(v)? => Ok(true),
        Some(_) => Err(Error::Der),
        None => Ok(false),
    }
}

/// Parses `issuingDistributionPoint`.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_idp(v: &[u8]) -> Result<IssuingDistributionPoint, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let name = r.read_optional(tag::context(0))?.map(read_dp_name).transpose()?;
    let only_user = flag(&mut r, 1)?;
    let only_ca = flag(&mut r, 2)?;
    let only_some_reasons =
        r.read_optional(tag::context_primitive(3))?.map(der::named_bits_mask).transpose()?;
    let indirect = flag(&mut r, 4)?;
    let only_attribute = flag(&mut r, 5)?;
    r.finish()?;
    if u8::from(only_user) + u8::from(only_ca) + u8::from(only_attribute) > 1 {
        return Err(Error::Der);
    }
    Ok(IssuingDistributionPoint {
        name,
        only_user,
        only_ca,
        only_some_reasons,
        indirect,
        only_attribute,
    })
}

/// The `keyIdentifier` of `authorityKeyIdentifier`, if present.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_authority_key_id(v: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let id = r.read_optional(tag::context_primitive(0))?.map(<[u8]>::to_vec);
    r.read_optional(tag::context(1))?;
    r.read_optional(tag::context_primitive(2))?;
    r.finish()?;
    Ok(id)
}

/// Parses `extKeyUsage` into its purpose OIDs.
///
/// # Errors
///
/// [`Error::Der`] when the input is not that DER structure.
pub fn parse_ext_key_usage(v: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut r = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
    let mut out = Vec::new();
    while !r.is_empty() {
        out.push(der::oid(r.read(tag::OID)?)?.to_vec());
    }
    if out.is_empty() {
        return Err(Error::Der);
    }
    Ok(out)
}
