//! TLS 1.3 (RFC 8446) and TLS 1.2 (RFC 5246) as a state machine over bytes, without I/O: the caller
//! moves bytes between the connection and the socket, and supplies the time and the randomness.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

mod client;
mod client12;
pub mod codec;
pub mod config;
pub mod conn;
pub mod keyschedule;
pub mod kx;
pub mod msgs;
#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod negative12_tests;
#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod negative_tests;
pub mod record;
#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod rfc8448_replay;
mod server;
mod server12;
pub mod suites;
pub mod tls12;

pub use config::{
    CertifiedKey, ClientAuth, ClientConfig, ServerConfig, SniIdentity, Trust, Version, name_matches,
};
pub use conn::{Connection, ENTROPY_LEN, Ticket};

/// Alert descriptions (RFC 8446, 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alert {
    /// `close_notify`.
    CloseNotify,
    /// `unexpected_message`.
    UnexpectedMessage,
    /// `bad_record_mac`.
    BadRecordMac,
    /// `record_overflow`.
    RecordOverflow,
    /// `handshake_failure`.
    HandshakeFailure,
    /// `bad_certificate`.
    BadCertificate,
    /// `unsupported_certificate`.
    UnsupportedCertificate,
    /// `certificate_revoked`.
    CertificateRevoked,
    /// `certificate_expired`.
    CertificateExpired,
    /// `certificate_unknown`.
    CertificateUnknown,
    /// `illegal_parameter`.
    IllegalParameter,
    /// `unknown_ca`.
    UnknownCa,
    /// `decode_error`.
    DecodeError,
    /// `decrypt_error`.
    DecryptError,
    /// `protocol_version`.
    ProtocolVersion,
    /// `insufficient_security`.
    InsufficientSecurity,
    /// `inappropriate_fallback` (RFC 7507).
    InappropriateFallback,
    /// `internal_error`.
    InternalError,
    /// `user_canceled`.
    UserCanceled,
    /// `no_renegotiation` (TLS 1.2, RFC 5246).
    NoRenegotiation,
    /// `missing_extension`.
    MissingExtension,
    /// `unsupported_extension`.
    UnsupportedExtension,
    /// `unknown_psk_identity`.
    UnknownPskIdentity,
    /// `certificate_required`.
    CertificateRequired,
    /// `no_application_protocol`.
    NoApplicationProtocol,
    /// `bad_certificate_status_response` (RFC 8446, 6.2).
    BadCertificateStatusResponse,
    /// Any other description, as received.
    Other(u8),
}

const ALERT_CODES: [(Alert, u8); 26] = [
    (Alert::CloseNotify, 0),
    (Alert::UnexpectedMessage, 10),
    (Alert::BadRecordMac, 20),
    (Alert::RecordOverflow, 22),
    (Alert::HandshakeFailure, 40),
    (Alert::BadCertificate, 42),
    (Alert::UnsupportedCertificate, 43),
    (Alert::CertificateRevoked, 44),
    (Alert::CertificateExpired, 45),
    (Alert::CertificateUnknown, 46),
    (Alert::IllegalParameter, 47),
    (Alert::UnknownCa, 48),
    (Alert::DecodeError, 50),
    (Alert::DecryptError, 51),
    (Alert::ProtocolVersion, 70),
    (Alert::InsufficientSecurity, 71),
    (Alert::InappropriateFallback, 86),
    (Alert::InternalError, 80),
    (Alert::UserCanceled, 90),
    (Alert::NoRenegotiation, 100),
    (Alert::MissingExtension, 109),
    (Alert::UnsupportedExtension, 110),
    (Alert::UnknownPskIdentity, 115),
    (Alert::CertificateRequired, 116),
    (Alert::BadCertificateStatusResponse, 113),
    (Alert::NoApplicationProtocol, 120),
];

impl Alert {
    /// The description byte.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Alert::Other(c) => c,
            known => ALERT_CODES.iter().find(|(a, _)| *a == known).map_or(0, |(_, c)| *c),
        }
    }

    /// The alert of a description byte.
    #[must_use]
    pub fn from_code(code: u8) -> Alert {
        ALERT_CODES.iter().find(|(_, c)| *c == code).map_or(Alert::Other(code), |(a, _)| *a)
    }

    /// The alert for a certificate that does not validate.
    #[must_use]
    pub fn for_certificate(e: teseladb_x509::Error) -> Alert {
        use teseladb_x509::Error as X;
        match e {
            X::Expired | X::NotYetValid => Alert::CertificateExpired,
            X::Revoked => Alert::CertificateRevoked,
            X::UnknownIssuer => Alert::UnknownCa,
            X::UnsupportedAlgorithm => Alert::UnsupportedCertificate,
            X::Der | X::Pem | X::InvalidKey => Alert::BadCertificate,
            _ => Alert::CertificateUnknown,
        }
    }
}

/// Why a connection failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// This side detected the problem and sends this alert.
    Local(Alert),
    /// The peer sent this alert (`close_notify` included).
    Peer(Alert),
    /// The peer's certificate did not validate; the alert sent is derived from it.
    Certificate(teseladb_x509::Error),
    /// The connection is closed or failed earlier.
    Closed,
}

impl Error {
    /// The alert this side sends for the error, if any.
    #[must_use]
    pub fn alert(self) -> Option<Alert> {
        match self {
            Error::Local(a) => Some(a),
            Error::Certificate(e) => Some(Alert::for_certificate(e)),
            Error::Peer(_) | Error::Closed => None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Local(a) => write!(f, "TLS error, sent alert {}", a.code()),
            Error::Peer(a) => write!(f, "TLS peer sent alert {}", a.code()),
            Error::Certificate(e) => write!(f, "TLS peer certificate: {e}"),
            Error::Closed => f.write_str("TLS connection closed"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn alerts_and_errors() {
        use teseladb_x509::Error as X;

        for (a, c) in ALERT_CODES {
            assert_eq!(Alert::from_code(c), a);
            assert_eq!(a.code(), c);
        }
        assert_eq!(Alert::from_code(200), Alert::Other(200));
        assert_eq!(Alert::Other(200).code(), 200);
        for (e, a) in [
            (X::Expired, Alert::CertificateExpired),
            (X::Revoked, Alert::CertificateRevoked),
            (X::UnknownIssuer, Alert::UnknownCa),
            (X::UnsupportedAlgorithm, Alert::UnsupportedCertificate),
            (X::Der, Alert::BadCertificate),
            (X::Policy, Alert::CertificateUnknown),
        ] {
            assert_eq!(Error::Certificate(e).alert(), Some(a));
        }
        assert_eq!(Error::Peer(Alert::CloseNotify).alert(), None);
        assert_eq!(Error::Closed.alert(), None);
        for e in [
            Error::Local(Alert::DecodeError),
            Error::Peer(Alert::CloseNotify),
            Error::Certificate(X::Expired),
            Error::Closed,
        ] {
            assert!(!e.to_string().is_empty());
        }
    }
}
