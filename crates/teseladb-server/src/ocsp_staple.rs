//! OCSP stapling of the clients' port (013 FR-031), as the reference server 9.0.4 staples (measured
//! on 31-10-2026): when the port's certificate names an OCSP responder in its
//! `authorityInfoAccess`, the node asks it over HTTP, checks the answer against the issuer and
//! staples a `good` answer that has a `nextUpdate`; without one it warns and staples nothing. The
//! answer is asked for again halfway to its `nextUpdate`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use teseladb_http::client::{Scheme, Url, post};
use teseladb_http::http1::{Limits, ResponseReader};
use teseladb_tls::ServerConfig;
use teseladb_x509::Certificate;
use teseladb_x509::ocsp::{self, CertId, Status};

use crate::log::{Pending, defer, defer_at};
use crate::tls_port::TlsPort;

/// How long the node waits for the responder.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// When an answer could not be had, how soon the node asks again.
pub const RETRY_SECS: i64 = 60;
/// The content type of an OCSP request over HTTP (RFC 6960, appendix A.1).
const OCSP_REQUEST: &str = "application/ocsp-request";
/// HTTP status of a successful answer.
const HTTP_OK: u16 = 200;
/// The reference server's identifiers of its log lines (measured).
const ID_FETCH_STATUS: i32 = 577_163;
const ID_NO_NEXT_UPDATE: i32 = 6_144_500;

/// Asks `url` (HTTP) about `id`; the DER answer.
pub fn fetch(url: &str, id: &CertId, timeout: Duration) -> Result<Vec<u8>, String> {
    let target = Url::parse(url).map_err(|e| format!("OCSP responder {url}: {e:?}"))?;
    if target.scheme != Scheme::Http {
        return Err(format!("OCSP responder {url}: not HTTP"));
    }
    let bytes = post(&target, crate::USER_AGENT, OCSP_REQUEST, &ocsp::request(id))
        .encode()
        .map_err(crate::error_debug)?;
    let addr = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(crate::error_text)?
        .next()
        .ok_or_else(|| format!("OCSP responder {url}: no address"))?;
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(crate::error_text)?;
    sock.set_read_timeout(Some(timeout)).map_err(crate::error_text)?;
    sock.write_all(&bytes).map_err(crate::error_text)?;
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 16 * 1024];
    let response = loop {
        let n = sock.read(&mut buf).map_err(crate::error_text)?;
        if n == 0 {
            break reader.finish().map_err(crate::error_debug)?;
        }
        if let Some(r) = reader.feed(&buf[..n]).map_err(crate::error_debug)? {
            break r;
        }
    };
    if response.status != HTTP_OK {
        return Err(format!("OCSP responder {url}: HTTP {}", response.status));
    }
    Ok(response.body)
}

/// What a stapling round gave: the answer to staple, if any, and when to ask again (seconds).
#[derive(Debug, PartialEq, Eq)]
pub struct Round {
    /// The answer to staple.
    pub staple: Option<Vec<u8>>,
    /// When to ask again.
    pub next_secs: i64,
}

/// One round for `leaf`, which `issuer` signed, at `now` (seconds), asking with `ask`.
pub fn round(
    leaf: &Certificate,
    issuer: &Certificate,
    now: i64,
    ask: &dyn Fn(&str, &CertId) -> Result<Vec<u8>, String>,
) -> Round {
    let retry = Round { staple: None, next_secs: now + RETRY_SECS };
    let Some(url) = ocsp::responder_urls(leaf).into_iter().next() else {
        return Round { staple: None, next_secs: i64::MAX };
    };
    let Ok(id) = CertId::new(leaf, issuer) else { return retry };
    let answer = ask(&url, &id).and_then(|der| {
        ocsp::check(&der, &id, issuer, now).map(|c| (der, c)).map_err(|e| e.to_string())
    });
    let status = |ok: bool| Pending {
        id: ID_FETCH_STATUS,
        msg: "OCSP fetch status".into(),
        attr: format!(r#"{{"status":{{"code":{}}}}}"#, i32::from(!ok)),
    };
    match answer {
        Err(_) => {
            defer(status(false));
            retry
        }
        Ok((der, c)) => {
            defer_at(crate::log::Level::Info, status(true));
            match (c.status, c.next_update) {
                (Status::Good, Some(next)) => {
                    Round { staple: Some(der), next_secs: now + (next - now).max(2) / 2 }
                }
                (Status::Good, None) => {
                    defer(Pending {
                        id: ID_NO_NEXT_UPDATE,
                        msg: "Server will not staple the OCSP response because it is missing a nextUpdate time.".into(),
                        attr: "{}".into(),
                    });
                    retry
                }
                _ => retry,
            }
        }
    }
}

/// `port` with `staple` (or none) in its TLS settings.
pub fn stapled(port: &TlsPort, staple: Option<Vec<u8>>) -> Option<TlsPort> {
    let config = port.config()?;
    let with = ServerConfig { ocsp_staple: staple, ..(*config).clone() };
    Some(TlsPort::new(port.mode, Some(with)))
}

/// Keeps `port`'s staple fresh in a thread of its own while `stop` says no, for a certificate
/// that names a responder.
pub fn spawn(port: TlsPort, issuer: Certificate, stop: fn() -> bool) {
    let Some(leaf) = port.config().and_then(|c| c.identity.chain.first().cloned()) else { return };
    if ocsp::responder_urls(&leaf).is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let ask = |url: &str, id: &CertId| fetch(url, id, FETCH_TIMEOUT);
        while !stop() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
            let r = round(&leaf, &issuer, now, &ask);
            if let Some(next) = stapled(&port, r.staple) {
                port.replace(&next);
            }
            let wait = (r.next_secs - now).clamp(1, RETRY_SECS * 60);
            for _ in 0..wait {
                if stop() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}
