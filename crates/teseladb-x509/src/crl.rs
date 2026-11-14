//! Certificate revocation lists (RFC 5280, section 5): parsing, scope checks against a
//! certificate's distribution points (6.3.3 (b)) and entry lookup, including indirect and delta
//! CRLs.

use crate::Error;
use crate::cert::Certificate;
use crate::der::{self, tag};
use crate::ext::{self, DistributionPoint, Extension, GeneralName, IssuingDistributionPoint};
use crate::key::SignatureAlgorithm;
use crate::name::Name;
use crate::oid;
use crate::pem;
use crate::time;

/// `CRLReason` value removeFromCRL (only meaningful in delta CRLs).
pub const REASON_REMOVE_FROM_CRL: u64 = 8;

/// One revoked certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokedEntry {
    /// The serial number's INTEGER contents.
    pub serial: Vec<u8>,
    /// `revocationDate`.
    pub date: i64,
    /// `reasonCode`, if present.
    pub reason: Option<u64>,
    /// The issuer of the revoked certificate when it is not the CRL issuer (indirect CRLs).
    pub certificate_issuer: Option<Vec<GeneralName>>,
}

/// A parsed CRL.
#[derive(Clone, Debug)]
pub struct Crl {
    der: Vec<u8>,
    tbs: (usize, usize),
    /// The signature algorithm.
    pub signature_algorithm: SignatureAlgorithm,
    /// The CRL issuer.
    pub issuer: Name,
    /// `thisUpdate`.
    pub this_update: i64,
    /// `nextUpdate`.
    pub next_update: Option<i64>,
    /// The revoked certificates.
    pub entries: Vec<RevokedEntry>,
    /// Every CRL extension.
    pub extensions: Vec<Extension>,
    /// `cRLNumber` magnitude.
    pub crl_number: Option<Vec<u8>>,
    /// `BaseCRLNumber` of `deltaCRLIndicator`: present only in delta CRLs.
    pub delta_base: Option<Vec<u8>>,
    /// `issuingDistributionPoint`.
    pub idp: Option<IssuingDistributionPoint>,
    /// `keyIdentifier` of `authorityKeyIdentifier`.
    pub authority_key_id: Option<Vec<u8>>,
    /// The signature.
    pub signature: Vec<u8>,
    /// Whether a critical CRL or entry extension is not processed here; such a CRL is unusable.
    pub unknown_critical: bool,
}

/// Compares two unsigned big-endian magnitudes.
#[must_use]
pub fn cmp_unsigned(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let strip = |x: &[u8]| -> Vec<u8> { x.iter().copied().skip_while(|&v| v == 0).collect() };
    let (a, b) = (strip(a), strip(b));
    a.len().cmp(&b.len()).then_with(|| a.cmp(&b))
}

fn parse_entries(entries: &[u8], unknown_critical: &mut bool) -> Result<Vec<RevokedEntry>, Error> {
    let mut out = Vec::new();
    let mut r = der::Reader::new(entries);
    let mut current_issuer: Option<Vec<GeneralName>> = None;
    while !r.is_empty() {
        let mut e = der::Reader::new(r.read(tag::SEQUENCE)?);
        let serial = der::integer(e.read(tag::INTEGER)?)?.to_vec();
        let date = time::read(&mut e)?;
        let mut reason = None;
        if let Some(v) = e.read_optional(tag::SEQUENCE)? {
            for x in ext::parse_extensions(v)? {
                match x.oid.as_slice() {
                    oid::REASON_CODE => {
                        reason = Some(der::small_unsigned(
                            der::read_single(&x.value, tag::ENUMERATED)?.value,
                        )?);
                    }
                    oid::INVALIDITY_DATE => {
                        time::read(&mut der::Reader::new(&x.value))?;
                    }
                    oid::CERTIFICATE_ISSUER => {
                        current_issuer = Some(ext::parse_general_names(
                            der::read_single(&x.value, tag::SEQUENCE)?.value,
                        )?);
                    }
                    _ => *unknown_critical |= x.critical,
                }
            }
        }
        e.finish()?;
        out.push(RevokedEntry { serial, date, reason, certificate_issuer: current_issuer.clone() });
    }
    Ok(out)
}

impl Crl {
    /// Parses a DER CRL.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] for a malformed CRL, and [`Error::DuplicateExtension`] or
    /// [`Error::UnknownCriticalExtension`] for its extensions.
    pub fn from_der(data: &[u8]) -> Result<Crl, Error> {
        let outer = der::read_single(data, tag::SEQUENCE)?;
        let mut r = der::Reader::new(outer.value);
        let tbs = r.read_tlv(tag::SEQUENCE)?;
        let outer_alg = r.read(tag::SEQUENCE)?;
        let signature = der::octet_aligned_bits(r.read(tag::BIT_STRING)?)?.to_vec();
        r.finish()?;
        let mut t = der::Reader::new(tbs.value);
        let version = t.read_optional(tag::INTEGER)?.map(der::small_unsigned).transpose()?;
        if version.is_some_and(|v| v != 1) {
            return Err(Error::Der);
        }
        let inner_alg = t.read(tag::SEQUENCE)?;
        if inner_alg != outer_alg {
            return Err(Error::Der);
        }
        let signature_algorithm = SignatureAlgorithm::from_der(inner_alg)?;
        let issuer = Name::from_der(t.read_tlv(tag::SEQUENCE)?.raw)?;
        let this_update = time::read(&mut t)?;
        let next_update = match t.peek_tag() {
            Some(tag::UTC_TIME | tag::GENERALIZED_TIME) => Some(time::read(&mut t)?),
            _ => None,
        };
        let mut unknown_critical = false;
        let entries = match t.read_optional(tag::SEQUENCE)? {
            Some(v) => parse_entries(v, &mut unknown_critical)?,
            None => Vec::new(),
        };
        let extensions = match t.read_optional(tag::context(0))? {
            Some(v) if version.is_some() => {
                ext::parse_extensions(der::read_single(v, tag::SEQUENCE)?.value)?
            }
            Some(_) => return Err(Error::Der),
            None => Vec::new(),
        };
        t.finish()?;
        let tbs_start = tbs.raw.as_ptr() as usize - data.as_ptr() as usize;
        let mut crl = Crl {
            der: data.to_vec(),
            tbs: (tbs_start, tbs_start + tbs.raw.len()),
            signature_algorithm,
            issuer,
            this_update,
            next_update,
            entries,
            extensions: Vec::new(),
            crl_number: None,
            delta_base: None,
            idp: None,
            authority_key_id: None,
            signature,
            unknown_critical,
        };
        for x in &extensions {
            let v = x.value.as_slice();
            match x.oid.as_slice() {
                oid::CRL_NUMBER => {
                    crl.crl_number =
                        Some(der::unsigned(der::read_single(v, tag::INTEGER)?.value)?.to_vec());
                }
                oid::DELTA_CRL_INDICATOR => {
                    crl.delta_base =
                        Some(der::unsigned(der::read_single(v, tag::INTEGER)?.value)?.to_vec());
                }
                oid::ISSUING_DISTRIBUTION_POINT => crl.idp = Some(ext::parse_idp(v)?),
                oid::AUTHORITY_KEY_ID => crl.authority_key_id = ext::parse_authority_key_id(v)?,
                oid::ISSUER_ALT_NAME | oid::FRESHEST_CRL => {}
                _ => crl.unknown_critical |= x.critical,
            }
        }
        crl.extensions = extensions;
        Ok(crl)
    }

    /// Every CRL in a PEM text.
    ///
    /// # Errors
    ///
    /// [`Error::Pem`] for malformed PEM, and the errors of [`Crl::from_der`] for each block.
    pub fn from_pem_all(text: &str) -> Result<Vec<Crl>, Error> {
        pem::parse_label(text, pem::label::CRL)?.iter().map(|d| Crl::from_der(d)).collect()
    }

    /// The DER encoding.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The DER `TBSCertList`.
    #[must_use]
    pub fn tbs(&self) -> &[u8] {
        &self.der[self.tbs.0..self.tbs.1]
    }

    /// Whether the CRL is current at `now`: issued, and its `nextUpdate`, if any, not yet reached.
    #[must_use]
    pub fn is_current(&self, now: i64) -> bool {
        self.this_update <= now && self.next_update.is_none_or(|n| n > now)
    }

    /// The reasons the CRL covers: `onlySomeReasons` or every reason.
    #[must_use]
    pub fn reasons(&self) -> u16 {
        self.idp.as_ref().and_then(|i| i.only_some_reasons).unwrap_or(ext::reasons::ALL)
    }

    /// RFC 5280, 6.3.3 (b): whether this CRL can answer for `cert` through `dp`.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when a distribution point name of the CRL or of `dp` is malformed.
    pub fn in_scope(&self, cert: &Certificate, dp: &DistributionPoint) -> Result<bool, Error> {
        let indirect = self.idp.as_ref().is_some_and(|i| i.indirect);
        let issuer_ok = if dp.crl_issuer.is_empty() {
            self.issuer == cert.issuer
        } else {
            indirect
                && dp
                    .crl_issuer
                    .iter()
                    .any(|g| matches!(g, GeneralName::Directory(n) if *n == self.issuer))
        };
        if !issuer_ok {
            return Ok(false);
        }
        let Some(idp) = &self.idp else {
            return Ok(true);
        };
        if let Some(idp_name) = &idp.name {
            let idp_names = idp_name.resolve(&self.issuer)?;
            let dp_names = match &dp.name {
                Some(n) => n.resolve(&self.issuer)?,
                None => dp.crl_issuer.clone(),
            };
            if !idp_names.iter().any(|a| dp_names.contains(a)) {
                return Ok(false);
            }
        }
        let is_ca = cert.is_ca();
        Ok(!(idp.only_user && is_ca || idp.only_ca && !is_ca || idp.only_attribute))
    }

    /// The entry for `cert`, if listed: the serial matches and the entry's certificate issuer
    /// (the CRL issuer unless an indirect CRL names another) is the certificate's issuer.
    #[must_use]
    pub fn find(&self, cert: &Certificate) -> Option<&RevokedEntry> {
        self.entries.iter().find(|e| {
            e.serial == cert.serial
                && match &e.certificate_issuer {
                    None => self.issuer == cert.issuer,
                    Some(names) => names
                        .iter()
                        .any(|g| matches!(g, GeneralName::Directory(n) if *n == cert.issuer)),
                }
        })
    }

    /// Whether `delta` is a delta CRL that applies to this complete CRL: same issuer and scope,
    /// and based on this CRL or an earlier one.
    #[must_use]
    pub fn accepts_delta(&self, delta: &Crl) -> bool {
        match (&delta.delta_base, &self.crl_number, &self.delta_base) {
            (Some(base), Some(number), None) => {
                delta.issuer == self.issuer
                    && delta.idp == self.idp
                    && cmp_unsigned(base, number).is_le()
            }
            _ => false,
        }
    }
}
