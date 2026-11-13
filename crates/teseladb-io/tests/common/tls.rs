//! Certificates of a cluster's CA for the tests of TLS between nodes (013 FR-006).

use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::{CertifiedKey, ClientAuth, ClientConfig, ServerConfig, Trust};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

use teseladb_io::secure::PeerTls;

pub const YEAR: i64 = 365 * 86_400;

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

pub fn ec(seed: u8) -> PrivateKey {
    PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).unwrap()
}

pub fn ca(seed: u8) -> (Certificate, PrivateKey) {
    let key = ec(seed);
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        now() - 60,
        now() + YEAR,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    (generate::self_signed(&p, &key).unwrap(), key)
}

pub fn node_identity(ca: &(Certificate, PrivateKey), seed: u8) -> CertifiedKey {
    let key = ec(seed);
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "node")]),
        now() - 60,
        now() + YEAR,
    );
    p.alt_names = vec![GeneralName::Ip(vec![127, 0, 0, 1])];
    let cert = generate::issue(&p, &key.public_key(), &ca.0, &ca.1).unwrap();
    CertifiedKey { chain: vec![cert, ca.0.clone()], key }
}

pub fn peer_tls(ca: &(Certificate, PrivateKey), seed: u8) -> PeerTls {
    let id = node_identity(ca, seed);
    let mut server = ServerConfig::new(id.clone());
    server.client_auth = ClientAuth::Optional;
    server.trust = Trust::anchors(vec![ca.0.clone()]);
    let mut client = ClientConfig::new(Trust::anchors(vec![ca.0.clone()]));
    client.identity = Some(id);
    PeerTls::new(server, client)
}
