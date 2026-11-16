//! What the tests of the network pieces on the engine share (023 FR-016, FR-024): certificates of
//! a test CA, TLS settings, listeners on loopback, and waiting on a thread from the shard.

#![allow(dead_code)]

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::net::api::{ConnectedSocket, ListenOptions, ServerSocket};
use teseladb_crypto::ec::NamedCurve;
use teseladb_io::engine::connect::socket_address;
use teseladb_tls::keyschedule::Secret;
use teseladb_tls::{CertifiedKey, ClientAuth, ClientConfig, ServerConfig, Trust, Version};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::ocsp;
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

pub const YEAR: i64 = 365 * 86_400;

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

pub fn ec(seed: u8) -> PrivateKey {
    PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).unwrap()
}

/// A CA.
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

/// A certificate of `ca` for `localhost` and 127.0.0.1, with its chain.
pub fn identity(ca: &(Certificate, PrivateKey), seed: u8) -> CertifiedKey {
    let key = ec(seed);
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "node")]),
        now() - 60,
        now() + YEAR,
    );
    p.alt_names = vec![GeneralName::Dns("localhost".into()), GeneralName::Ip(vec![127, 0, 0, 1])];
    let cert = generate::issue(&p, &key.public_key(), &ca.0, &ca.1).unwrap();
    CertifiedKey { chain: vec![cert, ca.0.clone()], key }
}

/// A server of `ca`'s certificate for TLS 1.2 and 1.3, as the clients' port, that asks for client
/// certificates of `ca` and issues tickets.
pub fn server_config(ca: &(Certificate, PrivateKey)) -> ServerConfig {
    let mut s = ServerConfig::new(identity(ca, 2));
    s.min_version = Version::Tls12;
    s.client_auth = ClientAuth::Optional;
    s.trust = Trust::anchors(vec![ca.0.clone()]);
    s.ticket_key = Some(Secret(vec![9; 32]));
    s
}

/// A good OCSP response of `ca` for the server certificate of [`server_config`] (or a revoked one).
pub fn ocsp_response(ca: &(Certificate, PrivateKey), revoked: bool) -> Vec<u8> {
    let leaf = identity(ca, 2).chain[0].clone();
    let id = ocsp::CertId::new(&leaf, &ca.0).unwrap();
    let status = if revoked { ocsp::Status::Revoked(now() - 30) } else { ocsp::Status::Good };
    ocsp::respond(&id, status, now() - 60, now() + 3600, &ca.1).unwrap()
}

/// A client that trusts `ca`, with a certificate of it.
pub fn client_config(ca: &(Certificate, PrivateKey)) -> ClientConfig {
    let mut c = ClientConfig::new(Trust::anchors(vec![ca.0.clone()]));
    c.identity = Some(identity(ca, 3));
    c
}

/// A listener of this shard on a free loopback port, and its address.
pub fn listen() -> (ServerSocket, SocketAddr) {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    // SAFETY: on this shard's reactor thread, for this call only.
    let ss = unsafe { (*engine()).net() }.listen(socket_address(any), opts).unwrap();
    let port = ss.local_address().unwrap().port();
    (ss, SocketAddr::new(any.ip(), port))
}

/// The next connection of `ss`.
pub async fn accept(ss: &mut ServerSocket) -> Result<ConnectedSocket, ExceptionPtr> {
    Ok(ss.accept().await?.connection)
}

/// `h`'s result, waited for without blocking the shard.
pub async fn join<T: Send + 'static>(h: JoinHandle<T>) -> T {
    while !h.is_finished() {
        let tick: Future<()> = sleep_steady(Duration::from_millis(1));
        tick.await.unwrap();
    }
    h.join().unwrap()
}

/// A blocking socket to `addr`, as the threads of the reference implementation use.
pub fn blocking(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s
}

pub fn arc<T>(t: T) -> Arc<T> {
    Arc::new(t)
}

/// `n` bytes of a pattern that does not repeat with any record size.
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(31) ^ (i >> 7)) as u8).collect()
}
