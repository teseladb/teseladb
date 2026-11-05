//! HTTPS requests of the node to the services that authentication needs (013 FR-025): the STS
//! of REFDB-AWS and the JWKS of REFDB-OIDC. Each request goes on its own connection with
//! [`teseladb_http`]'s messages over [`TlsStream`], trusting the system's certificate
//! authorities.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use teseladb_http::client::{Scheme, Url};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_tls::{ClientConfig, Trust};
use teseladb_x509::Certificate;

use crate::tls_stream::TlsStream;

/// The system's bundles of certificate authorities, the first that exists.
const SYSTEM_CAS: [&str; 4] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
];

/// The certificates of a bundle that parse. A system bundle can hold an old root the parser
/// refuses (macOS's `/etc/ssl/cert.pem` has EC-ACC, with a negative serial number, which RFC 5280
/// forbids); like Go's `AppendCertsFromPEM`, that root is left out and the others stay.
pub fn bundle(path: &str, text: &str) -> Result<Vec<Certificate>, String> {
    let ders =
        teseladb_x509::pem::parse_label(text, "CERTIFICATE").map_err(|e| format!("{path}: {e:?}"))?;
    let cas: Vec<Certificate> = ders.iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
    if cas.is_empty() {
        return Err(format!("{path}: no certificate"));
    }
    Ok(cas)
}

/// The system's certificate authorities.
pub fn system_trust() -> Result<Trust, String> {
    for path in SYSTEM_CAS {
        if let Ok(text) = std::fs::read_to_string(path) {
            return Ok(Trust::anchors(bundle(path, &text)?));
        }
    }
    Err("no bundle of certificate authorities on this system".into())
}

/// A client of one HTTPS service.
#[derive(Clone)]
pub struct Https {
    pub(crate) tls: Arc<ClientConfig>,
    timeout: Duration,
}

impl std::fmt::Debug for Https {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Https").field("timeout", &self.timeout).finish_non_exhaustive()
    }
}

impl Https {
    /// A client trusting `trust`, giving up after `timeout` on each step.
    pub fn new(trust: Trust, timeout: Duration) -> Https {
        let mut config = ClientConfig::new(trust);
        config.min_version = teseladb_tls::Version::Tls12;
        Https { tls: Arc::new(config), timeout }
    }

    /// The same client keeping only FIPS-approved algorithms when `on` (`--tlsFIPSMode`).
    pub fn fips(self, on: bool) -> Https {
        if !on {
            return self;
        }
        Https { tls: Arc::new(self.tls.as_ref().clone().fips()), ..self }
    }

    /// Sends `req` to `url` (HTTPS only) and reads the whole answer.
    pub fn send(&self, url: &Url, req: &Request) -> Result<Response, String> {
        if url.scheme != Scheme::Https {
            return Err(format!("{}: not HTTPS", url.authority()));
        }
        let bytes = req.encode().map_err(crate::error_debug)?;
        let addrs = (url.host.as_str(), url.port).to_socket_addrs().map_err(crate::error_text)?;
        let mut last = format!("{}: no address", url.authority());
        let mut sock = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.timeout) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        let sock = sock.ok_or(last)?;
        sock.set_read_timeout(Some(self.timeout)).map_err(crate::error_text)?;
        sock.set_write_timeout(Some(self.timeout)).map_err(crate::error_text)?;
        let mut tls = TlsStream::client(sock, self.tls.clone(), &url.host)?;
        tls.send(&bytes)?;
        let mut reader = ResponseReader::new(Limits::default());
        loop {
            let data = tls.recv()?;
            if data.is_empty() {
                return reader.finish().map_err(crate::error_debug);
            }
            if let Some(r) = reader.feed(&data).map_err(crate::error_debug)? {
                return Ok(r);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use teseladb_tls::{CertifiedKey, ServerConfig};
    use teseladb_x509::ext::GeneralName;

    #[test]
    fn fips_mode_keeps_only_approved_suites_and_groups() {
        let plain = Https::new(Trust::anchors(Vec::new()), Duration::from_secs(1));
        assert!(plain.tls.suites.iter().any(|s| !s.is_fips_approved()));
        let off = plain.fips(false);
        assert!(off.tls.groups.iter().any(|g| !g.is_fips_approved()));
        let on = off.fips(true);
        assert!(on.tls.suites.iter().all(|s| s.is_fips_approved()));
        assert!(on.tls.groups.iter().all(|g| g.is_fips_approved()));
        assert_eq!(on.tls.min_version, teseladb_tls::Version::Tls12);
    }

    #[test]
    fn a_bundle_keeps_the_certificates_that_parse() {
        let dir = std::env::temp_dir().join(format!("teseladb-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let params = crate::pki::PkiParams { now: 1_800_000_000, node_names: Vec::new() };
        let pki = crate::pki::load_or_create(&dir, &params, &mut |b| teseladb_sys::fill_random(b))
            .unwrap();
        let broken = teseladb_x509::pem::encode("CERTIFICATE", &[0x30, 0x03, 0x02, 0x01, 0x01]);
        let text = format!("{broken}{}", pki.ca.to_pem());
        let kept = bundle("b", &text).unwrap();
        assert_eq!(kept.iter().map(Certificate::der).collect::<Vec<_>>(), vec![pki.ca.der()]);
        assert!(bundle("b", &broken).unwrap_err().contains("no certificate"));
        assert!(
            bundle("b", "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n").is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that answers each request with `answer` over TLS.
    fn server(config: Arc<ServerConfig>, answer: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(sock) = sock else { return };
                let config = config.clone();
                std::thread::spawn(move || {
                    let Ok(mut s) = TlsStream::server(sock, config, &[]) else { return };
                    let mut got = Vec::new();
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.recv() {
                            Ok(d) if !d.is_empty() => got.extend_from_slice(&d),
                            _ => return,
                        }
                    }
                    let _ = s.send(answer);
                    s.close();
                });
            }
        });
        port
    }

    #[test]
    fn requests_go_over_tls_and_answers_are_read_whole() {
        let dir = std::env::temp_dir().join(format!("teseladb-https-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
        let params =
            crate::pki::PkiParams { now, node_names: vec![GeneralName::Dns("localhost".into())] };
        let pki = crate::pki::load_or_create(&dir, &params, &mut |b| teseladb_sys::fill_random(b))
            .unwrap();
        let config = Arc::new(ServerConfig::new(CertifiedKey {
            chain: pki.node_chain.clone(),
            key: pki.node_key.clone(),
        }));
        let https = Https::new(Trust::anchors(vec![pki.ca.clone()]), Duration::from_secs(5));
        assert!(format!("{https:?}").contains("Https"));
        let url = |port: u16| Url::parse(&format!("https://localhost:{port}/x")).unwrap();
        let req = |u: &Url| teseladb_http::client::get(u, crate::USER_AGENT);
        let port = server(config.clone(), b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let r = https.send(&url(port), &req(&url(port))).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"ok"[..]));
        // An answer that ends with the connection, and one that is not HTTP.
        let port = server(config.clone(), b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nall");
        assert_eq!(https.send(&url(port), &req(&url(port))).unwrap().body, b"all");
        let port = server(config, b"garbage\r\n\r\n");
        assert!(https.send(&url(port), &req(&url(port))).is_err());
        // Not HTTPS, nobody listening, a name with no address, and a request that does not encode.
        let plain = Url::parse("http://localhost:1/").unwrap();
        assert!(https.send(&plain, &req(&plain)).unwrap_err().contains("not HTTPS"));
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(https.send(&url(dead), &req(&url(dead))).is_err());
        let nowhere = Url::parse("https://no-such-host.invalid/").unwrap();
        assert!(https.send(&nowhere, &req(&nowhere)).is_err());
        let bad = Request::new("GET", "/");
        assert!(https.send(&url(dead), &bad).is_err());
        assert!(
            system_trust().is_ok() || SYSTEM_CAS.iter().all(|p| !std::path::Path::new(p).exists())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
