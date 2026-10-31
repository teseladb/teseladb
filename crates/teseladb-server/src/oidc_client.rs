//! The identity providers of REFDB-OIDC (013 FR-025): `--setParameter oidcIdentityProviders`
//! and the JWKS of each provider, found through its discovery document and read over HTTPS when
//! a login first needs it. A key the JWKS does not know reads it again, at most once every
//! `JWKSMinimumQuiescePeriodSecs` (60 seconds, RefDb's default).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use teseladb_auth::oidc::{self, KeySet, Providers};
use teseladb_http::client::{self, Url};

use crate::https::{Https, system_trust};

/// How long each step of a request to a provider may take.
const IDP_TIMEOUT: Duration = Duration::from_secs(10);
/// `JWKSMinimumQuiescePeriodSecs` by default.
pub const QUIESCE: Duration = Duration::from_secs(60);

/// The node's providers and the keys read from them.
#[derive(Debug)]
pub struct Oidc {
    /// `oidcIdentityProviders`.
    pub providers: Providers,
    https: Https,
    quiesce: Duration,
    keys: Mutex<HashMap<String, (Instant, KeySet)>>,
}

impl PartialEq for Oidc {
    /// The same providers.
    fn eq(&self, other: &Oidc) -> bool {
        self.providers == other.providers
    }
}

impl Eq for Oidc {}

impl Oidc {
    /// The providers of `json`, reached trusting the system's authorities; `fips` keeps only
    /// FIPS-approved algorithms on the TLS (`--tlsFIPSMode`).
    pub fn from_json(json: &str, fips: bool) -> Result<Oidc, String> {
        let https = Https::new(system_trust()?, IDP_TIMEOUT).fips(fips);
        Oidc::with_https(json, https, QUIESCE)
    }

    /// The providers of `json`, reached with `https`, reading a JWKS again at most every
    /// `quiesce`.
    pub fn with_https(json: &str, https: Https, quiesce: Duration) -> Result<Oidc, String> {
        let providers = Providers::parse(json).map_err(|e| e.0)?;
        Ok(Oidc { providers, https, quiesce, keys: Mutex::new(HashMap::new()) })
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let target = Url::parse(url).map_err(|e| format!("{url}: {e:?}"))?;
        let answer = self.https.send(&target, &client::get(&target))?;
        if answer.status != 200 {
            return Err(format!("{url}: HTTP {}", answer.status));
        }
        Ok(answer.body)
    }

    /// The JWKS of the provider `issuer`; with `refresh`, read again unless it was read less
    /// than the quiet period ago.
    pub fn keys(&self, issuer: &str, refresh: bool) -> Result<KeySet, String> {
        if self.providers.by_issuer(issuer).is_none() {
            return Err(format!("Unknown Identity Provider '{issuer}'"));
        }
        let lock = || self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, keys)) = lock().get(issuer)
            && (!refresh || at.elapsed() < self.quiesce)
        {
            return Ok(keys.clone());
        }
        let discovery =
            self.get(&format!("{}{}", issuer.trim_end_matches('/'), oidc::DISCOVERY_PATH))?;
        let uri = oidc::jwks_uri(&discovery).map_err(|e| e.0)?;
        let keys = KeySet::parse(&self.get(&uri)?).map_err(|e| e.0)?;
        lock().insert(issuer.to_owned(), (Instant::now(), keys.clone()));
        Ok(keys)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::tls_stream::TlsStream;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use teseladb_tls::{CertifiedKey, ServerConfig, Trust};
    use teseladb_x509::ext::GeneralName;

    /// A JWKS with the RSA key of the OIDC tests of `teseladb-auth` as `k1`.
    fn jwks() -> String {
        let pem = include_str!("../../teseladb-auth/tests/fixtures/oidc/rsa2048.pem");
        let teseladb_x509::PrivateKey::Rsa(key, _) =
            teseladb_x509::PrivateKey::from_pem(pem, None).unwrap()
        else {
            panic!("not RSA")
        };
        let b64 = |b: &[u8]| {
            teseladb_x509::pem::base64_encode(b)
                .trim_end_matches('=')
                .replace('+', "-")
                .replace('/', "_")
        };
        let p = key.public_key();
        format!(
            r#"{{"keys": [{{"kty": "RSA", "kid": "k1", "n": "{}", "e": "{}"}}]}}"#,
            b64(&p.modulus()),
            b64(&p.exponent())
        )
    }

    /// A provider: its discovery document and its JWKS (one RSA key `k1`), counting the reads.
    fn provider(config: Arc<ServerConfig>, reads: Arc<AtomicUsize>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(sock) = sock else { return };
                let (config, reads) = (config.clone(), reads.clone());
                std::thread::spawn(move || {
                    let Ok(mut s) = TlsStream::server(sock, config, &[]) else { return };
                    let mut got = Vec::new();
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.recv() {
                            Ok(d) if !d.is_empty() => got.extend_from_slice(&d),
                            _ => return,
                        }
                    }
                    reads.fetch_add(1, Ordering::SeqCst);
                    let head = String::from_utf8_lossy(&got).into_owned();
                    let body = if head.starts_with("GET /.well-known/openid-configuration ") {
                        format!(r#"{{"jwks_uri": "https://localhost:{port}/jwks"}}"#)
                    } else if head.starts_with("GET /jwks ") {
                        jwks()
                    } else {
                        let _ = s.send(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                        return;
                    };
                    let answer =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    let _ = s.send(answer.as_bytes());
                    s.close();
                });
            }
        });
        port
    }

    #[test]
    fn keys_are_read_through_discovery_and_kept() {
        let dir = std::env::temp_dir().join(format!("teseladb-oidc-{}", std::process::id()));
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
        let reads = Arc::new(AtomicUsize::new(0));
        let port = provider(config, reads.clone());
        let issuer = format!("https://localhost:{port}");
        let json = |iss: &str| {
            format!(
                r#"[{{"issuer": "{iss}", "audience": "a", "authNamePrefix": "p", "authorizationClaim": "g", "supportsHumanFlows": false}}]"#
            )
        };
        let https = Https::new(Trust::anchors(vec![pki.ca.clone()]), IDP_TIMEOUT);
        let o = Oidc::with_https(&json(&issuer), https.clone(), Duration::from_secs(600)).unwrap();
        assert_eq!(o, o);
        assert_eq!(o.keys(&issuer, false).unwrap().0.len(), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        // Kept, also for a refresh within the quiet period.
        o.keys(&issuer, false).unwrap();
        o.keys(&issuer, true).unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        // After it, a refresh reads again.
        let quick = Oidc::with_https(&json(&issuer), https.clone(), Duration::ZERO).unwrap();
        quick.keys(&issuer, false).unwrap();
        quick.keys(&issuer, true).unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 6);
        // An issuer not configured, one whose discovery is missing, a bad parameter.
        assert!(o.keys("https://other", false).unwrap_err().contains("Unknown Identity Provider"));
        let missing = format!("https://localhost:{port}/nothing");
        let o = Oidc::with_https(&json(&missing), https.clone(), QUIESCE).unwrap();
        assert!(o.keys(&missing, false).unwrap_err().contains("HTTP 404"));
        let bad_url = "https://localhost:port";
        let o = Oidc::with_https(&json(bad_url), https.clone(), QUIESCE).unwrap();
        assert!(o.keys(bad_url, false).is_err());
        assert!(Oidc::with_https("[1]", https, QUIESCE).is_err());
        assert!(Oidc::from_json("[]", false).is_ok() || crate::https::system_trust().is_err());
        assert!(Oidc::from_json("[]", true).is_ok() || crate::https::system_trust().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
