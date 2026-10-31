//! Client and server settings.

use teseladb_x509::{Certificate, Crl, CrlCheck, PrivateKey};

use crate::keyschedule::Secret;
use crate::suites::{CipherSuite, NamedGroup};
use crate::tls12::Tls12Suite;

/// Default lifetime of the session tickets a server issues: one day.
pub const DEFAULT_TICKET_LIFETIME: u32 = 86_400;
/// Longest ticket lifetime TLS 1.3 allows: seven days.
pub const MAX_TICKET_LIFETIME: u32 = 604_800;

/// The lowest protocol version a side accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Version {
    /// TLS 1.2 or 1.3 (what RefDb accepts from clients).
    Tls12,
    /// TLS 1.3 only (between nodes).
    Tls13,
}

/// A certificate chain, leaf first, with the leaf's private key.
#[derive(Clone, Debug)]
pub struct CertifiedKey {
    /// The chain sent to the peer, leaf first.
    pub chain: Vec<Certificate>,
    /// The leaf's private key.
    pub key: PrivateKey,
}

/// How the peer's certificate is validated.
#[derive(Clone, Debug)]
pub struct Trust {
    /// Trust anchors.
    pub anchors: Vec<Certificate>,
    /// CRLs to check.
    pub crls: Vec<Crl>,
    /// Which certificates need a CRL.
    pub crl_check: CrlCheck,
    /// SHA-256 of a CA's DER trusted without having it: a certificate of the chain the peer sends
    /// with that hash becomes an anchor (a node that enrolls knows only the fingerprint of the
    /// cluster's CA, 013 FR-005).
    pub pinned: Option<[u8; 32]>,
}

impl Trust {
    /// Trust in `anchors`, without revocation checking.
    pub fn anchors(anchors: Vec<Certificate>) -> Trust {
        Trust { anchors, crls: Vec::new(), crl_check: CrlCheck::None, pinned: None }
    }
}

/// Settings of a client.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// How the server's certificate is validated.
    pub trust: Trust,
    /// The client certificate, sent when the server asks for one.
    pub identity: Option<CertifiedKey>,
    /// Suites offered, in order of preference.
    pub suites: Vec<CipherSuite>,
    /// TLS 1.2 suites offered when `min_version` is TLS 1.2, in order of preference.
    pub tls12_suites: Vec<Tls12Suite>,
    /// Whether a TLS 1.2 server must use the extended master secret (RFC 7627; FIPS mode).
    pub require_extended_master_secret: bool,
    /// Groups offered; a key share goes for the first one.
    pub groups: Vec<NamedGroup>,
    /// ALPN protocols offered (empty: no extension).
    pub alpn: Vec<Vec<u8>>,
    /// Lowest version accepted.
    pub min_version: Version,
    /// Highest version offered: TLS 1.2 leaves TLS 1.3 out of the ClientHello.
    pub max_version: Version,
    /// Whether to ask for the server's OCSP response (`status_request`) and check it when the
    /// server staples one (013 FR-031).
    pub request_ocsp: bool,
}

impl ClientConfig {
    /// TLS 1.3 with every suite and group, trusting `trust`.
    pub fn new(trust: Trust) -> ClientConfig {
        ClientConfig {
            trust,
            identity: None,
            suites: CipherSuite::ALL.to_vec(),
            tls12_suites: Tls12Suite::ALL.to_vec(),
            require_extended_master_secret: false,
            groups: NamedGroup::ALL.to_vec(),
            alpn: Vec::new(),
            min_version: Version::Tls13,
            max_version: Version::Tls13,
            request_ocsp: true,
        }
    }

    /// The same settings limited to what FIPS 140-3 approves (`--tlsFIPSMode`, 013 FR-029); TLS
    /// 1.2 also needs the extended master secret (SP 800-135 and FIPS 140-3 IG D.Q).
    pub fn fips(self) -> ClientConfig {
        ClientConfig {
            suites: fips_suites(&self.suites),
            tls12_suites: fips_tls12_suites(&self.tls12_suites),
            require_extended_master_secret: true,
            groups: fips_groups(&self.groups),
            ..self
        }
    }
}

/// The AES-GCM suites of `suites`, in their order: ChaCha20-Poly1305 is not FIPS approved.
fn fips_suites(suites: &[CipherSuite]) -> Vec<CipherSuite> {
    suites.iter().copied().filter(|s| s.is_fips_approved()).collect()
}

/// The AES-GCM TLS 1.2 suites of `suites`, in their order.
fn fips_tls12_suites(suites: &[Tls12Suite]) -> Vec<Tls12Suite> {
    suites.iter().copied().filter(|s| s.is_fips_approved()).collect()
}

/// The NIST curves of `groups`, in their order: x25519 is not FIPS approved for key exchange.
fn fips_groups(groups: &[NamedGroup]) -> Vec<NamedGroup> {
    groups.iter().copied().filter(|g| g.is_fips_approved()).collect()
}

/// Whether a server asks for client certificates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientAuth {
    /// No CertificateRequest.
    None,
    /// Asks; a client without a certificate is accepted (`--tlsAllowConnectionsWithoutCertificates`).
    Optional,
    /// Asks and requires one.
    Required,
}

/// Settings of a server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The server's chain and key.
    pub identity: CertifiedKey,
    /// Whether client certificates are requested.
    pub client_auth: ClientAuth,
    /// How client certificates are validated.
    pub trust: Trust,
    /// Suites accepted, in order of preference.
    pub suites: Vec<CipherSuite>,
    /// TLS 1.2 suites accepted when `min_version` is TLS 1.2, in order of preference.
    pub tls12_suites: Vec<Tls12Suite>,
    /// Whether a TLS 1.2 client must offer the extended master secret (RFC 7627; FIPS mode).
    pub require_extended_master_secret: bool,
    /// Groups accepted, in order of preference.
    pub groups: Vec<NamedGroup>,
    /// ALPN protocols, in order of preference (empty: ALPN is ignored).
    pub alpn: Vec<Vec<u8>>,
    /// Key that protects session tickets; `None` issues no tickets and resumes nothing.
    pub ticket_key: Option<Secret>,
    /// Lifetime of issued tickets, seconds (at most seven days).
    pub ticket_lifetime: u32,
    /// Lowest version accepted.
    pub min_version: Version,
    /// Highest version accepted: TLS 1.2 answers every ClientHello with TLS 1.2.
    pub max_version: Version,
    /// The OCSP response stapled for clients that ask (`status_request`, 013 FR-031).
    pub ocsp_staple: Option<Vec<u8>>,
}

impl ServerConfig {
    /// TLS 1.3 with every suite and group, no client certificates and no tickets.
    pub fn new(identity: CertifiedKey) -> ServerConfig {
        ServerConfig {
            identity,
            client_auth: ClientAuth::None,
            trust: Trust::anchors(Vec::new()),
            suites: CipherSuite::ALL.to_vec(),
            tls12_suites: Tls12Suite::ALL.to_vec(),
            require_extended_master_secret: false,
            groups: NamedGroup::ALL.to_vec(),
            alpn: Vec::new(),
            ticket_key: None,
            ticket_lifetime: DEFAULT_TICKET_LIFETIME,
            min_version: Version::Tls13,
            max_version: Version::Tls13,
            ocsp_staple: None,
        }
    }

    /// The same settings limited to what FIPS 140-3 approves (`--tlsFIPSMode`, 013 FR-029); TLS
    /// 1.2 also needs the extended master secret (SP 800-135 and FIPS 140-3 IG D.Q).
    pub fn fips(self) -> ServerConfig {
        ServerConfig {
            suites: fips_suites(&self.suites),
            tls12_suites: fips_tls12_suites(&self.tls12_suites),
            require_extended_master_secret: true,
            groups: fips_groups(&self.groups),
            ..self
        }
    }
}
