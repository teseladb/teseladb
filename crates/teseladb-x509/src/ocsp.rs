//! OCSP (RFC 6960) for stapling and for checking a stapled response: the responder URL of a
//! certificate's `authorityInfoAccess`, the request for one certificate with a SHA-1 `CertID` (what
//! the reference server 9.0.4 staples, measured), and the Basic OCSP response read and checked
//! against the issuer: its signature (by the issuer, or by a responder certificate the issuer
//! signed for OCSP signing), the certificate it speaks of, its times and its status.

use crate::cert::Certificate;
use crate::der::{self, tag};
use crate::key::{PrivateKey, SignatureAlgorithm};
use crate::{Error, time};

/// `authorityInfoAccess`, 1.3.6.1.5.5.7.1.1.
pub const AUTHORITY_INFO_ACCESS: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x01];
/// `id-ad-ocsp`, 1.3.6.1.5.5.7.48.1.
pub const AD_OCSP: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01];
/// `id-pkix-ocsp-basic`, 1.3.6.1.5.5.7.48.1.1.
pub const OCSP_BASIC: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01, 0x01];
/// `id-kp-OCSPSigning`, 1.3.6.1.5.5.7.3.9.
pub const OCSP_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x09];
/// `uniformResourceIdentifier` of a `GeneralName`: [6] IMPLICIT `IA5String`.
const URI: u8 = 0x86;
/// `responseStatus` `successful`.
const SUCCESSFUL: u8 = 0;

/// The identifier of a certificate in OCSP: SHA-1 of its issuer's name and key, and its serial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertId {
    /// SHA-1 of the issuer's distinguished name, DER.
    pub issuer_name_hash: [u8; 20],
    /// SHA-1 of the issuer's public key bits.
    pub issuer_key_hash: [u8; 20],
    /// The certificate's serial number, as encoded.
    pub serial: Vec<u8>,
}

/// What the responder says of the certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// `good`.
    Good,
    /// `revoked`, at this time.
    Revoked(i64),
    /// `unknown`.
    Unknown,
}

/// A checked response for one certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The status.
    pub status: Status,
    /// `thisUpdate`, seconds.
    pub this_update: i64,
    /// `nextUpdate`, seconds, if the responder gave one.
    pub next_update: Option<i64>,
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(teseladb_crypto::HashAlgorithm::Sha1.hash(data).as_ref());
    out
}

/// The OCSP responder URLs of `cert`'s `authorityInfoAccess`.
#[must_use]
pub fn responder_urls(cert: &Certificate) -> Vec<String> {
    let Some(ext) = cert.extension(AUTHORITY_INFO_ACCESS) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Ok(list) = der::read_single(&ext.value, tag::SEQUENCE) else {
        return out;
    };
    let mut r = der::Reader::new(list.value);
    while let Ok(desc) = r.read(tag::SEQUENCE) {
        let mut d = der::Reader::new(desc);
        let (Ok(method), Ok(location)) = (d.read(tag::OID), d.read_any()) else {
            break;
        };
        if method == AD_OCSP && location.tag == URI {
            out.push(String::from_utf8_lossy(location.value).into_owned());
        }
    }
    out
}

impl CertId {
    /// The identifier of `cert`, which `issuer` signed.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when a certificate is malformed.
    pub fn new(cert: &Certificate, issuer: &Certificate) -> Result<CertId, Error> {
        let mut spki = der::Reader::new(der::read_single(&issuer.spki, tag::SEQUENCE)?.value);
        spki.read(tag::SEQUENCE)?;
        let (key, _) = der::bit_string(spki.read(tag::BIT_STRING)?)?;
        Ok(CertId {
            issuer_name_hash: sha1(cert.issuer.der()),
            issuer_key_hash: sha1(key),
            serial: cert.serial.clone(),
        })
    }

    fn to_der(&self) -> Vec<u8> {
        let alg = der::sequence(&[&der::object_id(crate::oid::SHA1), &der::null()]);
        der::sequence(&[
            &alg,
            &der::octets(&self.issuer_name_hash),
            &der::octets(&self.issuer_key_hash),
            &der::unsigned_integer(&self.serial),
        ])
    }

    fn matches(&self, der_id: &[u8]) -> bool {
        der_id == self.to_der()
    }
}

/// The `OCSPRequest` for one certificate, without a nonce.
#[must_use]
pub fn request(id: &CertId) -> Vec<u8> {
    let one = der::sequence(&[&id.to_der()]);
    let list = der::sequence(&[&one]);
    der::sequence(&[&der::sequence(&[&list])])
}

/// The `CertID` of the first request of an `OCSPRequest`, for a responder.
///
/// # Errors
///
/// [`Error::Der`] for a malformed request, and [`Error::UnsupportedAlgorithm`] for a `CertID` hashed with other than
/// SHA-1.
pub fn request_id(request: &[u8]) -> Result<CertId, Error> {
    let mut top = der::Reader::new(der::read_single(request, tag::SEQUENCE)?.value);
    let mut tbs = der::Reader::new(top.read(tag::SEQUENCE)?);
    tbs.read_optional(0xa0)?;
    tbs.read_optional(0xa1)?;
    let mut list = der::Reader::new(tbs.read(tag::SEQUENCE)?);
    let mut one = der::Reader::new(list.read(tag::SEQUENCE)?);
    let mut id = der::Reader::new(one.read(tag::SEQUENCE)?);
    let mut alg = der::Reader::new(id.read(tag::SEQUENCE)?);
    if der::oid(alg.read(tag::OID)?)? != crate::oid::SHA1 {
        return Err(Error::UnsupportedAlgorithm);
    }
    let hash = |r: &mut der::Reader<'_>| -> Result<[u8; 20], Error> {
        r.read(tag::OCTET_STRING)?.try_into().map_err(|_| Error::Der)
    };
    let issuer_name_hash = hash(&mut id)?;
    let issuer_key_hash = hash(&mut id)?;
    let serial = der::integer(id.read(tag::INTEGER)?)?.to_vec();
    id.finish()?;
    Ok(CertId { issuer_name_hash, issuer_key_hash, serial })
}

/// A successful `OCSPResponse` that says `status` of the certificate `id`, valid from `this_update` to `next_update`
/// (seconds) and signed by its `issuer` with `key`, the responder identified by its key.
///
/// # Errors
///
/// The errors of [`PrivateKey::sign`].
pub fn respond(
    id: &CertId,
    status: Status,
    this_update: i64,
    next_update: i64,
    key: &PrivateKey,
) -> Result<Vec<u8>, Error> {
    let cert_status = match status {
        Status::Good => der::tlv(0x80, &[]),
        Status::Revoked(at) => der::tlv(0xa1, &time::encode_generalized(at)),
        Status::Unknown => der::tlv(0x82, &[]),
    };
    let single = der::sequence(&[
        &id.to_der(),
        &cert_status,
        &time::encode_generalized(this_update),
        &der::tlv(0xa0, &time::encode_generalized(next_update)),
    ]);
    let tbs = der::sequence(&[
        &der::tlv(0xa2, &der::octets(&id.issuer_key_hash)),
        &time::encode_generalized(this_update),
        &der::sequence(&[&single]),
    ]);
    let signature = key.sign(&tbs)?;
    let basic = der::sequence(&[&tbs, &key.default_algorithm().to_der(), &der::bits(&signature)]);
    let bytes = der::sequence(&[&der::object_id(OCSP_BASIC), &der::octets(&basic)]);
    Ok(der::sequence(&[&der::tlv(tag::ENUMERATED, &[SUCCESSFUL]), &der::tlv(0xa0, &bytes)]))
}

/// The single response of a `ResponseData` that speaks of `id`.
fn single(responses: &[u8], id: &CertId) -> Result<Checked, Error> {
    let mut r = der::Reader::new(responses);
    while !r.is_empty() {
        let mut s = der::Reader::new(r.read(tag::SEQUENCE)?);
        let cert_id = s.read_tlv(tag::SEQUENCE)?;
        let status_tlv = s.read_any()?;
        let this_update = time::read(&mut s)?;
        let next_update = match s.read_optional(0xa0)? {
            Some(v) => {
                let mut n = der::Reader::new(v);
                Some(time::read(&mut n)?)
            }
            None => None,
        };
        if !id.matches(cert_id.raw) {
            continue;
        }
        let status = match status_tlv.tag {
            0x80 => Status::Good,
            0xa1 => {
                let mut info = der::Reader::new(status_tlv.value);
                Status::Revoked(time::read(&mut info)?)
            }
            0x82 => Status::Unknown,
            _ => return Err(Error::Der),
        };
        return Ok(Checked { status, this_update, next_update });
    }
    Err(Error::UnknownIssuer)
}

/// Reads `response` (an `OCSPResponse`) for the certificate `id` that `issuer` signed and checks
/// it at `now` (seconds): signed by the issuer or by a responder certificate the issuer signed
/// with OCSP signing in its extended key usage, `thisUpdate` not in the future and `nextUpdate`,
/// when given, not in the past.
///
/// # Errors
///
/// [`Error::Der`] for a malformed response, [`Error::UnsupportedAlgorithm`] for its signature algorithm,
/// [`Error::BadSignature`] or [`Error::UnknownIssuer`] when
/// neither the issuer nor an authorized responder signed it, [`Error::NotYetValid`] or
/// [`Error::Expired`] for its times, and [`Error::Revoked`] when it says the certificate is revoked.
pub fn check(
    response: &[u8],
    id: &CertId,
    issuer: &Certificate,
    now: i64,
) -> Result<Checked, Error> {
    let mut top = der::Reader::new(der::read_single(response, tag::SEQUENCE)?.value);
    let status = top.read(tag::ENUMERATED)?;
    if status != [SUCCESSFUL] {
        return Err(Error::Revoked);
    }
    let mut bytes = der::Reader::new(der::read_single(top.read(0xa0)?, tag::SEQUENCE)?.value);
    if bytes.read(tag::OID)? != OCSP_BASIC {
        return Err(Error::UnsupportedAlgorithm);
    }
    let basic = bytes.read(tag::OCTET_STRING)?;
    let mut b = der::Reader::new(der::read_single(basic, tag::SEQUENCE)?.value);
    let tbs = b.read_tlv(tag::SEQUENCE)?;
    let alg = SignatureAlgorithm::from_der(b.read(tag::SEQUENCE)?)?;
    let (signature, _) = der::bit_string(b.read(tag::BIT_STRING)?)?;
    let certs: Vec<Certificate> = match b.read_optional(0xa0)? {
        Some(v) => {
            let mut list = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
            let mut out = Vec::new();
            while !list.is_empty() {
                out.push(Certificate::from_der(list.read_tlv(tag::SEQUENCE)?.raw)?);
            }
            out
        }
        None => Vec::new(),
    };
    let signed_by = |key: &crate::PublicKey| key.verify(alg, tbs.raw, signature).is_ok();
    let delegated = certs.iter().any(|c| {
        c.verify_signed_by(&issuer.public_key).is_ok()
            && c.ext_key_usage.as_ref().is_some_and(|u| u.iter().any(|o| o == OCSP_SIGNING))
            && signed_by(&c.public_key)
    });
    if !signed_by(&issuer.public_key) && !delegated {
        return Err(Error::BadSignature);
    }
    let mut data = der::Reader::new(tbs.value);
    data.read_optional(0xa0)?;
    data.read_any()?;
    time::read(&mut data)?;
    let checked = single(data.read(tag::SEQUENCE)?, id)?;
    if checked.this_update > now {
        return Err(Error::NotYetValid);
    }
    if checked.next_update.is_some_and(|n| n < now) {
        return Err(Error::Expired);
    }
    Ok(checked)
}
