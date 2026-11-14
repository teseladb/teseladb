//! Certificate and CRL generation for the cluster's PKI: the self-signed
//! CA, node and client certificates signed by it, and CRLs. Randomness (serial numbers, keys)
//! comes from the caller.

use crate::Error;
use crate::cert::Certificate;
use crate::crl::Crl;
use crate::der::{self, tag};
use crate::ext::{Extension, GeneralName, key_usage};
use crate::key::{PrivateKey, PublicKey};
use crate::name::Name;
use crate::oid;
use crate::time;

/// Length of generated serial numbers, in bytes (RFC 5280 allows up to 20).
pub const SERIAL_LEN: usize = 16;

/// What a new certificate says.
#[derive(Clone, Debug)]
pub struct CertificateParams {
    /// Random bytes for the serial number; the top bit is cleared so that it is positive.
    pub serial: [u8; SERIAL_LEN],
    /// The subject.
    pub subject: Name,
    /// `notBefore`, seconds since the epoch.
    pub not_before: i64,
    /// `notAfter`, seconds since the epoch.
    pub not_after: i64,
    /// `Some(path_len)` for a CA certificate, `None` for an end entity.
    pub ca: Option<Option<u64>>,
    /// `keyUsage` bits; `None` chooses keyCertSign and cRLSign for a CA, digitalSignature
    /// otherwise.
    pub key_usage: Option<u16>,
    /// `extKeyUsage` purposes (empty: no extension).
    pub ext_key_usage: Vec<Vec<u8>>,
    /// `subjectAltName` entries (empty: no extension).
    pub alt_names: Vec<GeneralName>,
    /// Further extensions, written as given.
    pub extra: Vec<Extension>,
}

impl CertificateParams {
    /// An end-entity certificate for TLS servers and clients: digitalSignature, serverAuth and
    /// clientAuth.
    #[must_use]
    pub fn node(
        serial: [u8; SERIAL_LEN],
        subject: Name,
        not_before: i64,
        not_after: i64,
    ) -> CertificateParams {
        CertificateParams {
            serial,
            subject,
            not_before,
            not_after,
            ca: None,
            key_usage: None,
            ext_key_usage: vec![oid::SERVER_AUTH.to_vec(), oid::CLIENT_AUTH.to_vec()],
            alt_names: Vec::new(),
            extra: Vec::new(),
        }
    }
}

/// RFC 5280, 4.2.1.2, method 1: SHA-1 of the subject public key bits.
#[must_use]
pub fn key_identifier(key: &PublicKey) -> [u8; 20] {
    teseladb_crypto::sha1(&key.key_bytes())
}

/// The key identifier a certificate issued by `issuer` names: its own, or one computed from its
/// key when it has none.
fn authority_id(issuer: &Certificate) -> Vec<u8> {
    issuer.subject_key_id.clone().unwrap_or_else(|| key_identifier(&issuer.public_key).to_vec())
}

fn serial_der(raw: &[u8; SERIAL_LEN]) -> Vec<u8> {
    let mut s = *raw;
    s[0] &= 0x7f;
    // A zero serial is not positive; the last byte makes it at least 1.
    s[SERIAL_LEN - 1] |= u8::from(s.iter().all(|&b| b == 0));
    der::unsigned_integer(&s)
}

fn extensions(
    params: &CertificateParams,
    subject_key: &PublicKey,
    issuer_key_id: &[u8],
) -> Vec<Extension> {
    let mut out = Vec::new();
    let bc = match params.ca {
        Some(Some(len)) => der::sequence(&[&der::boolean_value(true), &der::small_integer(len)]),
        Some(None) => der::sequence(&[&der::boolean_value(true)]),
        None => der::sequence(&[]),
    };
    out.push(Extension { oid: oid::BASIC_CONSTRAINTS.to_vec(), critical: true, value: bc });
    let default_usage = match params.ca {
        Some(_) => key_usage::KEY_CERT_SIGN | key_usage::CRL_SIGN,
        None => key_usage::DIGITAL_SIGNATURE,
    };
    let ku = der::named_bits(params.key_usage.unwrap_or(default_usage));
    out.push(Extension { oid: oid::KEY_USAGE.to_vec(), critical: true, value: ku });
    if !params.ext_key_usage.is_empty() {
        let oids: Vec<Vec<u8>> = params.ext_key_usage.iter().map(|o| der::object_id(o)).collect();
        let parts: Vec<&[u8]> = oids.iter().map(Vec::as_slice).collect();
        out.push(Extension {
            oid: oid::EXT_KEY_USAGE.to_vec(),
            critical: false,
            value: der::sequence(&parts),
        });
    }
    if !params.alt_names.is_empty() {
        let names: Vec<Vec<u8>> = params.alt_names.iter().map(GeneralName::to_der).collect();
        let parts: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        out.push(Extension {
            oid: oid::SUBJECT_ALT_NAME.to_vec(),
            // RFC 5280, 4.2.1.6: critical when the subject is empty.
            critical: params.subject.is_empty(),
            value: der::sequence(&parts),
        });
    }
    let ski = key_identifier(subject_key);
    out.push(Extension {
        oid: oid::SUBJECT_KEY_ID.to_vec(),
        critical: false,
        value: der::octets(&ski),
    });
    out.push(Extension {
        oid: oid::AUTHORITY_KEY_ID.to_vec(),
        critical: false,
        value: der::sequence(&[&der::tlv(tag::context_primitive(0), issuer_key_id)]),
    });
    out.extend(params.extra.iter().cloned());
    out
}

fn signed(tbs: &[u8], key: &PrivateKey) -> Result<Vec<u8>, Error> {
    let sig = key.sign(tbs)?;
    Ok(der::sequence(&[tbs, &key.default_algorithm().to_der(), &der::bits(&sig)]))
}

fn build(
    params: &CertificateParams,
    subject_key: &PublicKey,
    issuer: &Name,
    issuer_key_id: &[u8],
    signer: &PrivateKey,
) -> Result<Certificate, Error> {
    if params.not_after < params.not_before {
        return Err(Error::Expired);
    }
    let exts: Vec<Vec<u8>> =
        extensions(params, subject_key, issuer_key_id).iter().map(Extension::to_der).collect();
    let ext_parts: Vec<&[u8]> = exts.iter().map(Vec::as_slice).collect();
    let tbs = der::sequence(&[
        &der::tlv(tag::context(0), &der::small_integer(2)),
        &serial_der(&params.serial),
        &signer.default_algorithm().to_der(),
        issuer.der(),
        &der::sequence(&[&time::encode(params.not_before), &time::encode(params.not_after)]),
        params.subject.der(),
        &subject_key.to_spki(),
        &der::tlv(tag::context(3), &der::sequence(&ext_parts)),
    ]);
    Certificate::from_der(&signed(&tbs, signer)?)
}

/// A self-signed certificate (the cluster CA when `params.ca` is set).
///
/// # Errors
///
/// The errors of [`crate::PrivateKey::sign`].
pub fn self_signed(params: &CertificateParams, key: &PrivateKey) -> Result<Certificate, Error> {
    let public = key.public_key();
    let id = key_identifier(&public);
    build(params, &public, &params.subject, &id, key)
}

/// A certificate for `subject_key` signed by `issuer` with `issuer_key`.
///
/// # Errors
///
/// [`Error::InvalidKey`] when `issuer_key` is not the key of `issuer`, and the errors of
/// [`crate::PrivateKey::sign`].
pub fn issue(
    params: &CertificateParams,
    subject_key: &PublicKey,
    issuer: &Certificate,
    issuer_key: &PrivateKey,
) -> Result<Certificate, Error> {
    if issuer_key.public_key() != issuer.public_key {
        return Err(Error::InvalidKey);
    }
    let id = authority_id(issuer);
    build(params, subject_key, &issuer.subject, &id, issuer_key)
}

/// One revoked certificate of a new CRL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revocation {
    /// The certificate's serial (INTEGER contents, as `Certificate::serial`).
    pub serial: Vec<u8>,
    /// Revocation date, seconds since the epoch.
    pub date: i64,
}

/// A v2 CRL from `issuer`, with `cRLNumber` and `authorityKeyIdentifier`.
///
/// # Errors
///
/// [`Error::InvalidKey`] when `issuer_key` is not the key of `issuer`, and the errors of
/// [`crate::PrivateKey::sign`].
pub fn issue_crl(
    issuer: &Certificate,
    issuer_key: &PrivateKey,
    this_update: i64,
    next_update: i64,
    number: u64,
    revoked: &[Revocation],
) -> Result<Crl, Error> {
    if issuer_key.public_key() != issuer.public_key {
        return Err(Error::InvalidKey);
    }
    let entries: Vec<Vec<u8>> = revoked
        .iter()
        .map(|r| der::sequence(&[&der::tlv(tag::INTEGER, &r.serial), &time::encode(r.date)]))
        .collect();
    let entry_parts: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
    let id = authority_id(issuer);
    let exts = [
        Extension {
            oid: oid::CRL_NUMBER.to_vec(),
            critical: false,
            value: der::small_integer(number),
        },
        Extension {
            oid: oid::AUTHORITY_KEY_ID.to_vec(),
            critical: false,
            value: der::sequence(&[&der::tlv(tag::context_primitive(0), &id)]),
        },
    ];
    let ext_der: Vec<Vec<u8>> = exts.iter().map(Extension::to_der).collect();
    let ext_parts: Vec<&[u8]> = ext_der.iter().map(Vec::as_slice).collect();
    let mut tbs_parts: Vec<Vec<u8>> = vec![
        der::small_integer(1),
        issuer_key.default_algorithm().to_der(),
        issuer.subject.der().to_vec(),
        time::encode(this_update),
        time::encode(next_update),
    ];
    if !entries.is_empty() {
        tbs_parts.push(der::sequence(&entry_parts));
    }
    tbs_parts.push(der::tlv(tag::context(0), &der::sequence(&ext_parts)));
    let parts: Vec<&[u8]> = tbs_parts.iter().map(Vec::as_slice).collect();
    let tbs = der::sequence(&parts);
    Crl::from_der(&signed(&tbs, issuer_key)?)
}
