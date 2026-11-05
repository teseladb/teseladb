//! TLS on the clients' port (013 FR-007, FR-034): the `--tlsMode` of a port and, per
//! connection, the transport that sits between the socket and the wire protocol. `requireTLS`
//! speaks only TLS; `allowTLS` and `preferTLS` take TLS when the first byte is a TLS handshake
//! record and plain bytes otherwise; `disabled` takes only plain bytes.

use std::sync::{Arc, PoisonError, RwLock};

use teseladb_tls::{CertifiedKey, ClientAuth, Connection, ENTROPY_LEN, ServerConfig, Trust, Version};
use teseladb_x509::{Certificate, Crl, CrlCheck, PrivateKey};

use crate::cli::{TLS_CA_FILE, TLS_CERTIFICATE_KEY_FILE, TLS_CRL_FILE, TlsOpts};
use crate::pki::Pki;

/// First byte of a TLS handshake record.
const TLS_HANDSHAKE: u8 = 22;

/// `--tlsMode` (`net.tls.mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TlsMode {
    /// No TLS.
    Disabled,
    /// Plain and TLS connections are taken; the node connects out without TLS.
    AllowTls,
    /// Plain and TLS connections are taken; the node connects out with TLS.
    PreferTls,
    /// Only TLS.
    RequireTls,
}

const MODES: [(TlsMode, &str); 4] = [
    (TlsMode::Disabled, "disabled"),
    (TlsMode::AllowTls, "allowTLS"),
    (TlsMode::PreferTls, "preferTLS"),
    (TlsMode::RequireTls, "requireTLS"),
];

impl TlsMode {
    /// The mode of a `--tlsMode` value.
    pub fn parse(text: &str) -> Option<TlsMode> {
        MODES.iter().find(|(_, s)| *s == text).map(|(m, _)| *m)
    }

    /// The `--tlsMode` value.
    pub fn as_str(self) -> &'static str {
        MODES.iter().find(|(m, _)| *m == self).map_or("", |(_, s)| s)
    }
}

/// The TLS settings of a port. The clones share the server side: [`TlsPort::replace`] rotates
/// it for the connections that come after (`rotateCertificates`, 013 FR-006), and those already
/// open keep theirs.
#[derive(Clone, Debug)]
pub struct TlsPort {
    /// The mode.
    pub mode: TlsMode,
    /// The server side of TLS; `None` with `disabled`.
    config: Option<Arc<RwLock<Arc<ServerConfig>>>>,
    /// The protocols this port offers on top of the shared server side, if any, with the copy
    /// of the server side that offers them.
    alpn: Option<Arc<Alpn>>,
}

/// The protocols a port offers (019 FR-001), and the server side with them made from the shared
/// one it was made from last: made again only when a rotation changed the shared one.
#[derive(Debug)]
struct Alpn {
    protocols: Vec<Vec<u8>>,
    made: std::sync::Mutex<Option<(Arc<ServerConfig>, Arc<ServerConfig>)>>,
}

impl Alpn {
    fn over(&self, base: Arc<ServerConfig>) -> Arc<ServerConfig> {
        let mut made = self.made.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((from, with)) = made.as_ref()
            && Arc::ptr_eq(from, &base)
        {
            return Arc::clone(with);
        }
        let mut c = (*base).clone();
        c.alpn.clone_from(&self.protocols);
        let with = Arc::new(c);
        *made = Some((base, Arc::clone(&with)));
        with
    }
}

impl TlsPort {
    /// The port in `mode` with the server side `config`.
    pub fn new(mode: TlsMode, config: Option<ServerConfig>) -> TlsPort {
        TlsPort { mode, config: config.map(|c| Arc::new(RwLock::new(Arc::new(c)))), alpn: None }
    }

    /// The server side of TLS now; `None` with `disabled`.
    pub fn config(&self) -> Option<Arc<ServerConfig>> {
        // A writer that panicked left a whole value behind: the change is one assignment.
        let base =
            self.config.as_ref().map(|c| c.read().unwrap_or_else(PoisonError::into_inner).clone());
        match (&self.alpn, base) {
            (Some(alpn), Some(base)) => Some(alpn.over(base)),
            (_, base) => base,
        }
    }

    /// A port of the same mode and server side that offers the protocols `alpn` (the HTTP
    /// port's `h2` and `http/1.1`, 019 FR-001). It shares the server side: a rotation of this
    /// port reaches it too (013 FR-036).
    #[must_use]
    pub fn with_alpn(&self, alpn: &[Vec<u8>]) -> TlsPort {
        let alpn = Alpn { protocols: alpn.to_vec(), made: std::sync::Mutex::new(None) };
        TlsPort { mode: self.mode, config: self.config.clone(), alpn: Some(Arc::new(alpn)) }
    }

    /// Puts the server side of `other` in place of this one, in every clone; a port without TLS
    /// keeps none.
    pub fn replace(&self, other: &TlsPort) {
        if let (Some(slot), Some(new)) = (&self.config, other.config()) {
            *slot.write().unwrap_or_else(PoisonError::into_inner) = new;
        }
    }
}

impl Default for TlsPort {
    fn default() -> TlsPort {
        TlsPort { mode: TlsMode::Disabled, config: None, alpn: None }
    }
}

impl PartialEq for TlsPort {
    /// The same mode, the same shared settings and the same protocols.
    fn eq(&self, other: &TlsPort) -> bool {
        let protocols = |p: &TlsPort| p.alpn.as_ref().map(|a| a.protocols.clone());
        self.mode == other.mode
            && protocols(self) == protocols(other)
            && match (&self.config, &other.config) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for TlsPort {}

/// How one connection's bytes travel.
pub enum Transport {
    /// Plain bytes.
    Plain,
    /// Waiting for the first byte to choose (`allowTLS`, `preferTLS`).
    Undecided(Arc<ServerConfig>),
    /// TLS, starting with the handshake.
    Tls(Box<Connection>),
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn tls(config: &Arc<ServerConfig>) -> Option<Transport> {
    let mut entropy = [0u8; ENTROPY_LEN];
    teseladb_sys::fill_random(&mut entropy).ok()?;
    Some(Transport::Tls(Box::new(Connection::server(config.clone(), now_ms(), &entropy))))
}

impl Transport {
    /// The transport of a new connection; `None` if the randomness for TLS is not available.
    pub fn new(port: &TlsPort) -> Option<Transport> {
        match (port.mode, port.config()) {
            (TlsMode::Disabled, _) | (_, None) => Some(Transport::Plain),
            (TlsMode::RequireTls, Some(c)) => tls(&c),
            (TlsMode::AllowTls | TlsMode::PreferTls, Some(c)) => Some(Transport::Undecided(c)),
        }
    }

    /// Takes bytes from the socket: what the wire protocol reads goes to `plain`, what goes back
    /// to the socket (the TLS handshake, alerts) to `out`. `false` closes the connection.
    pub fn incoming(&mut self, raw: &[u8], plain: &mut Vec<u8>, out: &mut Vec<u8>) -> bool {
        if let (Transport::Undecided(c), Some(&first)) = (&*self, raw.first()) {
            let next = if first == TLS_HANDSHAKE { tls(c) } else { Some(Transport::Plain) };
            match next {
                Some(t) => *self = t,
                None => return false,
            }
        }
        match self {
            Transport::Plain => {
                plain.extend_from_slice(raw);
                true
            }
            Transport::Undecided(_) => true,
            Transport::Tls(c) => {
                let ok = c.feed(raw).is_ok();
                out.extend_from_slice(&c.take_output());
                plain.extend_from_slice(&c.read());
                // close_notify: the client is done.
                ok && !c.peer_closed()
            }
        }
    }

    /// Sends wire protocol bytes; `false` closes the connection.
    pub fn outgoing(&mut self, data: &[u8], out: &mut Vec<u8>) -> bool {
        match self {
            Transport::Tls(c) => {
                let ok = c.write(data).is_ok();
                out.extend_from_slice(&c.take_output());
                ok
            }
            Transport::Plain | Transport::Undecided(_) => {
                out.extend_from_slice(data);
                true
            }
        }
    }

    /// Whether the connection is TLS and its handshake has finished.
    pub fn tls_established(&self) -> bool {
        matches!(self, Transport::Tls(c) if !c.is_handshaking())
    }

    /// The protocol ALPN agreed over TLS, if any.
    pub fn alpn(&self) -> Option<&[u8]> {
        match self {
            Transport::Tls(c) => c.alpn_protocol(),
            Transport::Plain | Transport::Undecided(_) => None,
        }
    }

    /// Whether the connection carries plain bytes: no TLS on the port, or a client that did not
    /// open with a handshake on a port that allows it.
    pub fn is_plain(&self) -> bool {
        matches!(self, Transport::Plain)
    }

    /// The client's certificate chain, leaf first, if it sent one over TLS.
    pub fn peer_certificates(&self) -> &[Certificate] {
        match self {
            Transport::Tls(c) => c.peer_certificates(),
            Transport::Plain | Transport::Undecided(_) => &[],
        }
    }
}

fn read(path: &std::path::Path, option: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("--{option} {}: {e}", path.display()))
}

/// The TLS of the clients' port from the options of `teseladb start`: the certificate key file
/// or the node certificate of the PKI; with `--tlsCAFile`, client certificates required (or
/// optional with `--tlsAllowConnectionsWithoutCertificates`); with `--tlsCRLFile`, the leaf
/// checked against it. TLS 1.2 and 1.3, as `refnode` accepts from clients.
pub fn build(opts: &TlsOpts, pki: &Pki) -> Result<TlsPort, String> {
    let mode = opts.mode.unwrap_or(TlsMode::RequireTls);
    if mode == TlsMode::Disabled {
        return Ok(TlsPort::new(mode, None));
    }
    let identity = match &opts.certificate_key_file {
        None => CertifiedKey { chain: pki.node_chain.clone(), key: pki.node_key.clone() },
        Some(path) => {
            let text = read(path, TLS_CERTIFICATE_KEY_FILE)?;
            let bad =
                |what: &str| format!("--{TLS_CERTIFICATE_KEY_FILE} {}: {what}", path.display());
            let password = opts.certificate_key_file_password.as_ref().map(|p| p.0.as_bytes());
            let chain = Certificate::from_pem_all(&text).map_err(|e| bad(&e.to_string()))?;
            let key = PrivateKey::from_pem(&text, password).map_err(|e| bad(&e.to_string()))?;
            if chain.first().is_none_or(|leaf| leaf.public_key != key.public_key()) {
                return Err(bad("the first certificate does not match the key"));
            }
            CertifiedKey { chain, key }
        }
    };
    let mut config = ServerConfig::new(identity);
    config.min_version = Version::Tls12;
    if let Some(path) = &opts.ca_file {
        let anchors = Certificate::from_pem_all(&read(path, TLS_CA_FILE)?)
            .ok()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| format!("--{TLS_CA_FILE} {}: no certificate", path.display()))?;
        config.trust = Trust::anchors(anchors);
        config.client_auth = if opts.allow_without_certificates {
            ClientAuth::Optional
        } else {
            ClientAuth::Required
        };
    }
    if let Some(path) = &opts.crl_file {
        let crls = Crl::from_pem_all(&read(path, TLS_CRL_FILE)?)
            .ok()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| format!("--{TLS_CRL_FILE} {}: no CRL", path.display()))?;
        config.trust.crls = crls;
        config.trust.crl_check = CrlCheck::Leaf;
    }
    Ok(TlsPort::new(mode, Some(fips(config, opts.fips_mode))))
}

/// The TLS of the administration port (013 FR-017): the node certificate, and client
/// certificates asked for but optional (`/healthz` needs none), trusted when the cluster's CA or
/// the `--tlsCAFile` of the clients' port signed them and checked against `--tlsCRLFile`.
pub fn admin(opts: &TlsOpts, pki: &Pki) -> Result<TlsPort, String> {
    let clients = build(&TlsOpts { mode: Some(TlsMode::RequireTls), ..opts.clone() }, pki)?;
    let mut config = ServerConfig::new(CertifiedKey {
        chain: pki.node_chain.clone(),
        key: pki.node_key.clone(),
    });
    config.min_version = Version::Tls12;
    config.client_auth = ClientAuth::Optional;
    config.trust.anchors.push(pki.ca.clone());
    if let Some(c) = clients.config() {
        config.trust.anchors.extend(c.trust.anchors.iter().cloned());
        config.trust.crls.clone_from(&c.trust.crls);
        config.trust.crl_check = c.trust.crl_check;
    }
    Ok(TlsPort::new(TlsMode::RequireTls, Some(fips(config, opts.fips_mode))))
}

/// `config` limited to FIPS-approved algorithms when `on` (`--tlsFIPSMode`, 013 FR-029).
fn fips(config: ServerConfig, on: bool) -> ServerConfig {
    if on { config.fips() } else { config }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_x509::Name;
    use teseladb_x509::generate::{self, CertificateParams};

    fn config(seed: u8) -> ServerConfig {
        let key = PrivateKey::ed25519(&[seed; 32]);
        let p = CertificateParams::node(
            [1; 16],
            Name::build(&[(teseladb_x509::oid::COMMON_NAME, "s")]),
            0,
            1 << 33,
        );
        let cert = generate::self_signed(&p, &key).unwrap();
        ServerConfig::new(CertifiedKey { chain: vec![cert], key })
    }

    #[test]
    fn ports_compare_by_mode_and_shared_settings() {
        let a = TlsPort::new(TlsMode::RequireTls, Some(config(1)));
        assert_eq!(a, a.clone());
        assert_ne!(a, TlsPort::new(TlsMode::RequireTls, Some(config(1))));
        assert_ne!(a, TlsPort { mode: TlsMode::AllowTls, ..a.clone() });
        assert_ne!(a, TlsPort::default());
        assert_eq!(TlsPort::default(), TlsPort::default());
    }

    /// 013 FR-036: the HTTP port's protocols ride on the shared server side, so a rotation of
    /// the clients' port reaches it; its copy is made again only after a rotation.
    #[test]
    fn a_port_with_protocols_shares_the_rotations_of_its_base() {
        let base = TlsPort::new(TlsMode::RequireTls, Some(config(1)));
        let protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let http = base.with_alpn(&protocols);
        assert_ne!(http, base);
        assert_eq!(http, http.clone());
        let first = http.config().unwrap();
        assert_eq!(first.alpn, protocols);
        assert!(base.config().unwrap().alpn.is_empty(), "the base keeps its own");
        assert!(Arc::ptr_eq(&first, &http.config().unwrap()), "no copy without a rotation");
        base.replace(&TlsPort::new(TlsMode::RequireTls, Some(config(2))));
        let rotated = http.config().unwrap();
        assert_eq!(rotated.identity.key.public_key(), PrivateKey::ed25519(&[2; 32]).public_key());
        assert_eq!(rotated.alpn, protocols);
        // Without TLS there is nothing to offer them on.
        assert!(TlsPort::default().with_alpn(&protocols).config().is_none());
    }

    #[test]
    fn a_replaced_port_serves_the_new_settings_in_every_clone() {
        let a = TlsPort::new(TlsMode::RequireTls, Some(config(1)));
        let clone = a.clone();
        let key = |p: &TlsPort| p.config().unwrap().identity.key.public_key();
        a.replace(&TlsPort::new(TlsMode::RequireTls, Some(config(2))));
        assert_eq!(key(&clone), PrivateKey::ed25519(&[2; 32]).public_key());
        // A port without TLS keeps none, and takes nothing from one without.
        let plain = TlsPort::default();
        plain.replace(&a);
        assert!(plain.config().is_none());
        a.replace(&plain);
        assert_eq!(key(&a), PrivateKey::ed25519(&[2; 32]).public_key());
    }

    #[test]
    fn transports_without_tls_have_no_peer() {
        let plain = Transport::new(&TlsPort::default()).unwrap();
        assert!(plain.peer_certificates().is_empty());
        let mut undecided =
            Transport::new(&TlsPort::new(TlsMode::PreferTls, Some(config(1)))).unwrap();
        assert!(undecided.peer_certificates().is_empty());
        let (mut plain_in, mut out) = (Vec::new(), Vec::new());
        assert!(undecided.incoming(&[], &mut plain_in, &mut out));
        assert!(undecided.outgoing(b"x", &mut out));
        assert_eq!(out, b"x");
        let tls = Transport::new(&TlsPort::new(TlsMode::RequireTls, Some(config(1)))).unwrap();
        assert!(tls.peer_certificates().is_empty());
    }
}
