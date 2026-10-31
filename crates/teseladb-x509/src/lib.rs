//! X.509 certificates of TeselaDB (013 FR-002), written from RFC 5280, X.690, RFC 7468, RFC 5958,
//! RFC 8018 and RFC 2986: strict DER and PEM, keys, certificates, CRLs and CSRs, certification
//! path validation (RFC 5280, section 6) with policies, name constraints and revocation, and the
//! generation of the cluster's CA and node certificates.
//!
//! Every function is pure: the current time comes in as an argument and nothing reads the clock,
//! files or the network.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod cert;
pub mod chain;
pub mod crl;
pub mod csr;
pub mod der;
pub mod ext;
pub mod generate;
pub mod ip;
pub mod key;
pub mod name;
pub mod ocsp;
pub mod oid;
pub mod pem;
mod policy;
pub mod time;

pub use cert::Certificate;
pub use chain::{CrlCheck, Purpose, VerifyOptions, verify_chain, verify_hostname};
pub use crl::Crl;
pub use key::{PrivateKey, PublicKey, SignatureAlgorithm};
pub use name::Name;
pub use policy::PolicySettings;

/// Why an encoding was rejected or a certificate path did not validate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// Malformed DER, or a structure that does not follow its ASN.1 definition.
    Der,
    /// Malformed PEM or Base64, or a PEM block with an unexpected label.
    Pem,
    /// An algorithm, key type or curve TeselaDB does not implement (DSA, for example).
    UnsupportedAlgorithm,
    /// A key that does not match its algorithm or is not valid for it.
    InvalidKey,
    /// The password of an encrypted private key is wrong, or its padding is.
    Decrypt,
    /// A signature that does not verify.
    BadSignature,
    /// A certificate whose `notBefore` is after the validation time.
    NotYetValid,
    /// A certificate whose `notAfter` is before the validation time.
    Expired,
    /// No chain from the certificate to a trust anchor.
    UnknownIssuer,
    /// A certificate used as a CA without `basicConstraints` `cA` true.
    NotCa,
    /// A chain longer than a `pathLenConstraint` or the configured maximum depth allows.
    PathLength,
    /// A key used for something its `keyUsage` does not allow.
    KeyUsage,
    /// A certificate whose `extKeyUsage` does not allow the purpose.
    ExtKeyUsage,
    /// A name outside the permitted subtrees or inside the excluded ones.
    NameConstraints,
    /// A critical extension TeselaDB does not process.
    UnknownCriticalExtension,
    /// The same extension twice in one certificate or CRL.
    DuplicateExtension,
    /// The certificate policies of the path do not satisfy the explicit policy requirement.
    Policy,
    /// The certificate is on a CRL of its issuer.
    Revoked,
    /// No valid CRL covers the certificate, so its status cannot be determined.
    RevocationUnknown,
    /// The host name or address is not among the certificate's subject alternative names.
    HostnameMismatch,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Error::Der => "malformed DER",
            Error::Pem => "malformed PEM",
            Error::UnsupportedAlgorithm => "unsupported algorithm",
            Error::InvalidKey => "invalid key",
            Error::Decrypt => "cannot decrypt the private key",
            Error::BadSignature => "certificate signature failure",
            Error::NotYetValid => "certificate is not yet valid",
            Error::Expired => "certificate has expired",
            Error::UnknownIssuer => "unable to get issuer certificate",
            Error::NotCa => "invalid CA certificate",
            Error::PathLength => "path length constraint exceeded",
            Error::KeyUsage => "key usage does not include the required use",
            Error::ExtKeyUsage => "unsupported certificate purpose",
            Error::NameConstraints => "name constraints violation",
            Error::UnknownCriticalExtension => "unhandled critical extension",
            Error::DuplicateExtension => "duplicate extension",
            Error::Policy => "no explicit policy",
            Error::Revoked => "certificate revoked",
            Error::RevocationUnknown => "unable to get certificate CRL",
            Error::HostnameMismatch => "hostname mismatch",
        };
        f.write_str(text)
    }
}

impl std::error::Error for Error {}
