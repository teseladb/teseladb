//! PKCS #10 certification requests (RFC 2986): the CSR a joining node sends with its new key
//!. Only the `extensionRequest` attribute is read, for the subject alternative
//! names; other attributes are skipped.

use crate::Error;
use crate::der::{self, tag};
use crate::ext::{self, Extension, GeneralName};
use crate::key::{PrivateKey, PublicKey, SignatureAlgorithm};
use crate::name::Name;
use crate::oid;
use crate::pem;

/// A parsed certification request.
#[derive(Clone, Debug)]
pub struct CertificateRequest {
    der: Vec<u8>,
    /// The requested subject.
    pub subject: Name,
    /// The key to certify.
    pub public_key: PublicKey,
    /// Subject alternative names from `extensionRequest`.
    pub alt_names: Vec<GeneralName>,
    /// The signature algorithm.
    pub signature_algorithm: SignatureAlgorithm,
}

impl CertificateRequest {
    /// Parses a DER request and checks its signature with its own key (proof of possession).
    ///
    /// # Errors
    ///
    /// [`Error::Der`] for a malformed request, and the errors of [`crate::PublicKey::from_spki`] and
    /// [`crate::SignatureAlgorithm::from_der`] for its key and algorithm.
    pub fn from_der(data: &[u8]) -> Result<CertificateRequest, Error> {
        let outer = der::read_single(data, tag::SEQUENCE)?;
        let mut r = der::Reader::new(outer.value);
        let info = r.read_tlv(tag::SEQUENCE)?;
        let signature_algorithm = SignatureAlgorithm::from_der(r.read(tag::SEQUENCE)?)?;
        let signature = der::octet_aligned_bits(r.read(tag::BIT_STRING)?)?.to_vec();
        r.finish()?;
        let mut i = der::Reader::new(info.value);
        if der::small_unsigned(i.read(tag::INTEGER)?)? != 0 {
            return Err(Error::Der);
        }
        let subject = Name::from_der(i.read_tlv(tag::SEQUENCE)?.raw)?;
        let public_key = PublicKey::from_spki(i.read_tlv(tag::SEQUENCE)?.raw)?;
        let attrs = i.read(tag::context(0))?;
        i.finish()?;
        let mut alt_names = Vec::new();
        let mut a = der::Reader::new(attrs);
        while !a.is_empty() {
            let mut attr = der::Reader::new(a.read(tag::SEQUENCE)?);
            let t = der::oid(attr.read(tag::OID)?)?;
            let values = attr.read(tag::SET)?;
            attr.finish()?;
            if t == oid::EXTENSION_REQUEST {
                let exts = ext::parse_extensions(der::read_single(values, tag::SEQUENCE)?.value)?;
                if let Some(san) = exts.iter().find(|e| e.oid == oid::SUBJECT_ALT_NAME) {
                    alt_names = ext::parse_general_names(
                        der::read_single(&san.value, tag::SEQUENCE)?.value,
                    )?;
                }
            }
        }
        public_key.verify(signature_algorithm, info.raw, &signature)?;
        Ok(CertificateRequest {
            der: data.to_vec(),
            subject,
            public_key,
            alt_names,
            signature_algorithm,
        })
    }

    /// Builds and signs a request for `key`.
    ///
    /// # Errors
    ///
    /// The errors of [`crate::PrivateKey::sign`].
    pub fn build(
        subject: &Name,
        key: &PrivateKey,
        alt_names: &[GeneralName],
    ) -> Result<CertificateRequest, Error> {
        let mut attrs = Vec::new();
        if !alt_names.is_empty() {
            let names: Vec<Vec<u8>> = alt_names.iter().map(GeneralName::to_der).collect();
            let parts: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
            let san = Extension {
                oid: oid::SUBJECT_ALT_NAME.to_vec(),
                critical: false,
                value: der::sequence(&parts),
            };
            let exts = der::sequence(&[&san.to_der()]);
            attrs = der::sequence(&[
                &der::object_id(oid::EXTENSION_REQUEST),
                &der::constructed(tag::SET, &[&exts]),
            ]);
        }
        let info = der::sequence(&[
            &der::small_integer(0),
            subject.der(),
            &key.public_key().to_spki(),
            &der::tlv(tag::context(0), &attrs),
        ]);
        let sig = key.sign(&info)?;
        let der = der::sequence(&[&info, &key.default_algorithm().to_der(), &der::bits(&sig)]);
        CertificateRequest::from_der(&der)
    }

    /// The DER encoding.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The PEM encoding.
    #[must_use]
    pub fn to_pem(&self) -> String {
        pem::encode(pem::label::CERTIFICATE_REQUEST, &self.der)
    }
}
