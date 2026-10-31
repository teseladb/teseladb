//! X.509 v1–v3 certificates (RFC 5280, section 4).

use crate::Error;
use crate::der::{self, tag};
use crate::ext::{
    self, BasicConstraints, DistributionPoint, Extension, GeneralName, NameConstraints,
};
use crate::key::{PublicKey, SignatureAlgorithm};
use crate::name::Name;
use crate::oid;
use crate::pem;
use crate::time;

/// Longest serial number accepted, in contents octets (20 plus a sign octet, RFC 5280 4.1.2.2).
const MAX_SERIAL_LEN: usize = 21;

/// A parsed certificate. The fields mirror `TBSCertificate`; the extensions that path
/// validation uses are decoded, every extension is kept in `extensions`.
#[derive(Clone, Debug)]
pub struct Certificate {
    der: Vec<u8>,
    tbs: (usize, usize),
    /// 1, 2 or 3.
    pub version: u8,
    /// The serial number's INTEGER contents (minimal, may start with a sign octet).
    pub serial: Vec<u8>,
    /// The signature algorithm (the same inside and outside `TBSCertificate`).
    pub signature_algorithm: SignatureAlgorithm,
    /// The issuer.
    pub issuer: Name,
    /// The subject.
    pub subject: Name,
    /// `notBefore`, in seconds since the epoch.
    pub not_before: i64,
    /// `notAfter`, in seconds since the epoch.
    pub not_after: i64,
    /// The subject public key.
    pub public_key: PublicKey,
    /// The DER SubjectPublicKeyInfo.
    pub spki: Vec<u8>,
    /// Every extension, in order.
    pub extensions: Vec<Extension>,
    /// `basicConstraints`.
    pub basic_constraints: Option<BasicConstraints>,
    /// `keyUsage`, as a mask of `ext::key_usage` bits.
    pub key_usage: Option<u16>,
    /// `extKeyUsage`.
    pub ext_key_usage: Option<Vec<Vec<u8>>>,
    /// `subjectAltName`.
    pub subject_alt_names: Vec<GeneralName>,
    /// `nameConstraints`.
    pub name_constraints: Option<NameConstraints>,
    /// `subjectKeyIdentifier`.
    pub subject_key_id: Option<Vec<u8>>,
    /// `keyIdentifier` of `authorityKeyIdentifier`.
    pub authority_key_id: Option<Vec<u8>>,
    /// `certificatePolicies`, if present.
    pub policies: Option<Vec<Vec<u8>>>,
    /// `policyMappings`.
    pub policy_mappings: Vec<(Vec<u8>, Vec<u8>)>,
    /// `requireExplicitPolicy` of `policyConstraints`.
    pub require_explicit_policy: Option<u64>,
    /// `inhibitPolicyMapping` of `policyConstraints`.
    pub inhibit_policy_mapping: Option<u64>,
    /// `inhibitAnyPolicy`.
    pub inhibit_any_policy: Option<u64>,
    /// `cRLDistributionPoints`.
    pub crl_distribution_points: Vec<DistributionPoint>,
    /// The signature, as the BIT STRING's bytes.
    pub signature: Vec<u8>,
    /// Whether an extension marked critical is not among those processed here.
    pub unknown_critical: bool,
}

impl Certificate {
    /// Parses a DER certificate.
    pub fn from_der(data: &[u8]) -> Result<Certificate, Error> {
        let outer = der::read_single(data, tag::SEQUENCE)?;
        let mut r = der::Reader::new(outer.value);
        let tbs = r.read_tlv(tag::SEQUENCE)?;
        let outer_alg = r.read(tag::SEQUENCE)?;
        let signature = der::octet_aligned_bits(r.read(tag::BIT_STRING)?)?.to_vec();
        r.finish()?;
        let tbs_start = tbs.raw.as_ptr() as usize - data.as_ptr() as usize;
        let mut cert = parse_tbs(tbs.value, outer_alg)?;
        cert.der = data.to_vec();
        cert.tbs = (tbs_start, tbs_start + tbs.raw.len());
        cert.signature = signature;
        Ok(cert)
    }

    /// Every certificate in a PEM text, in order.
    pub fn from_pem_all(text: &str) -> Result<Vec<Certificate>, Error> {
        pem::parse_label(text, pem::label::CERTIFICATE)?
            .iter()
            .map(|d| Certificate::from_der(d))
            .collect()
    }

    /// The DER encoding.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The DER `TBSCertificate`, the signed part.
    pub fn tbs(&self) -> &[u8] {
        &self.der[self.tbs.0..self.tbs.1]
    }

    /// The PEM encoding.
    pub fn to_pem(&self) -> String {
        pem::encode(pem::label::CERTIFICATE, &self.der)
    }

    /// Whether `issuer`'s key verifies this certificate's signature.
    pub fn verify_signed_by(&self, issuer: &PublicKey) -> Result<(), Error> {
        issuer.verify(self.signature_algorithm, self.tbs(), &self.signature)
    }

    /// Whether subject and issuer are the same name (RFC 5280, 6.1: a self-issued certificate).
    pub fn is_self_issued(&self) -> bool {
        self.subject == self.issuer
    }

    /// Whether the certificate may act as a CA: v3 with `cA` true. Version 1 and 2 certificates
    /// only act as CAs when they are trust anchors.
    pub fn is_ca(&self) -> bool {
        self.basic_constraints.is_some_and(|b| b.ca)
    }

    /// The extension with this OID, if present.
    pub fn extension(&self, ext_oid: &[u8]) -> Option<&Extension> {
        self.extensions.iter().find(|e| e.oid == ext_oid)
    }

    /// SHA-256 of the DER encoding (the CA fingerprint of the enrollment tokens).
    pub fn fingerprint_sha256(&self) -> [u8; 32] {
        teseladb_crypto::sha256(&self.der)
    }
}

fn parse_tbs(tbs: &[u8], outer_alg: &[u8]) -> Result<Certificate, Error> {
    let mut r = der::Reader::new(tbs);
    let version = match r.read_optional(tag::context(0))? {
        Some(v) => match der::small_unsigned(der::read_single(v, tag::INTEGER)?.value)? {
            // DER omits the DEFAULT v1.
            1 => 2,
            2 => 3,
            _ => return Err(Error::Der),
        },
        None => 1,
    };
    let serial = der::integer(r.read(tag::INTEGER)?)?;
    if serial[0] & 0x80 != 0 || serial.len() > MAX_SERIAL_LEN {
        return Err(Error::Der);
    }
    let inner_alg = r.read(tag::SEQUENCE)?;
    if inner_alg != outer_alg {
        return Err(Error::Der);
    }
    let signature_algorithm = SignatureAlgorithm::from_der(inner_alg)?;
    let issuer = Name::from_der(r.read_tlv(tag::SEQUENCE)?.raw)?;
    let mut validity = der::Reader::new(r.read(tag::SEQUENCE)?);
    let not_before = time::read(&mut validity)?;
    let not_after = time::read(&mut validity)?;
    validity.finish()?;
    let subject = Name::from_der(r.read_tlv(tag::SEQUENCE)?.raw)?;
    let spki = r.read_tlv(tag::SEQUENCE)?.raw.to_vec();
    let public_key = PublicKey::from_spki(&spki)?;
    for n in [1, 2] {
        if r.read_optional(tag::context_primitive(n))?.is_some() && version == 1 {
            return Err(Error::Der);
        }
    }
    let extensions = match r.read_optional(tag::context(3))? {
        Some(v) if version == 3 => {
            let list = ext::parse_extensions(der::read_single(v, tag::SEQUENCE)?.value)?;
            if list.is_empty() {
                return Err(Error::Der);
            }
            list
        }
        Some(_) => return Err(Error::Der),
        None => Vec::new(),
    };
    r.finish()?;
    let mut cert = Certificate {
        der: Vec::new(),
        tbs: (0, 0),
        version,
        serial: serial.to_vec(),
        signature_algorithm,
        issuer,
        subject,
        not_before,
        not_after,
        public_key,
        spki,
        extensions: Vec::new(),
        basic_constraints: None,
        key_usage: None,
        ext_key_usage: None,
        subject_alt_names: Vec::new(),
        name_constraints: None,
        subject_key_id: None,
        authority_key_id: None,
        policies: None,
        policy_mappings: Vec::new(),
        require_explicit_policy: None,
        inhibit_policy_mapping: None,
        inhibit_any_policy: None,
        crl_distribution_points: Vec::new(),
        signature: Vec::new(),
        unknown_critical: false,
    };
    for e in &extensions {
        decode_extension(&mut cert, e)?;
    }
    cert.extensions = extensions;
    Ok(cert)
}

fn decode_extension(cert: &mut Certificate, e: &Extension) -> Result<(), Error> {
    let v = e.value.as_slice();
    match e.oid.as_slice() {
        oid::BASIC_CONSTRAINTS => cert.basic_constraints = Some(ext::parse_basic_constraints(v)?),
        oid::KEY_USAGE => {
            let mask = der::named_bits_mask(der::read_single(v, tag::BIT_STRING)?.value)?;
            if mask == 0 {
                return Err(Error::Der);
            }
            cert.key_usage = Some(mask);
        }
        oid::EXT_KEY_USAGE => cert.ext_key_usage = Some(ext::parse_ext_key_usage(v)?),
        oid::SUBJECT_ALT_NAME => {
            cert.subject_alt_names =
                ext::parse_general_names(der::read_single(v, tag::SEQUENCE)?.value)?;
        }
        oid::ISSUER_ALT_NAME => {
            ext::parse_general_names(der::read_single(v, tag::SEQUENCE)?.value)?;
        }
        oid::NAME_CONSTRAINTS => cert.name_constraints = Some(ext::parse_name_constraints(v)?),
        oid::SUBJECT_KEY_ID => {
            cert.subject_key_id = Some(der::read_single(v, tag::OCTET_STRING)?.value.to_vec())
        }
        oid::AUTHORITY_KEY_ID => cert.authority_key_id = ext::parse_authority_key_id(v)?,
        oid::CERTIFICATE_POLICIES => cert.policies = Some(ext::parse_policies(v)?),
        oid::POLICY_MAPPINGS => cert.policy_mappings = ext::parse_policy_mappings(v)?,
        oid::POLICY_CONSTRAINTS => {
            let (require, inhibit) = ext::parse_policy_constraints(v)?;
            cert.require_explicit_policy = require;
            cert.inhibit_policy_mapping = inhibit;
        }
        oid::INHIBIT_ANY_POLICY => {
            cert.inhibit_any_policy =
                Some(der::small_unsigned(der::read_single(v, tag::INTEGER)?.value)?);
        }
        oid::CRL_DISTRIBUTION_POINTS => {
            cert.crl_distribution_points = ext::parse_distribution_points(v)?
        }
        _ => cert.unknown_critical |= e.critical,
    }
    Ok(())
}
