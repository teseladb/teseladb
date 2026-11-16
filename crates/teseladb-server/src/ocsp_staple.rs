//! OCSP stapling of the clients' port (013 FR-031), as the reference server 9.0.4 staples (measured
//! on 31-10-2026): when the port's certificate names an OCSP responder in its
//! `authorityInfoAccess`, the node asks it over HTTP, checks the answer against the issuer and
//! staples a `good` answer that has a `nextUpdate`; without one it warns and staples nothing. The
//! answer is asked for again halfway to its `nextUpdate`.

use std::time::Duration;

use teseladb_http::client::{Scheme, Url, post};
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

/// The responder of `url` and the bytes of the request about `id`.
fn ask_about(url: &str, id: &CertId) -> Result<(Url, Vec<u8>), String> {
    let target = Url::parse(url).map_err(|e| format!("OCSP responder {url}: {e:?}"))?;
    if target.scheme != Scheme::Http {
        return Err(format!("OCSP responder {url}: not HTTP"));
    }
    let bytes = post(&target, crate::USER_AGENT, OCSP_REQUEST, &ocsp::request(id))
        .encode()
        .map_err(crate::error_debug)?;
    Ok((target, bytes))
}

/// The DER answer of a responder's HTTP answer.
fn answer_of(url: &str, status: u16, body: Vec<u8>) -> Result<Vec<u8>, String> {
    if status != HTTP_OK {
        return Err(format!("OCSP responder {url}: HTTP {status}"));
    }
    Ok(body)
}

/// Asks `url` (HTTP) about `id` on this shard, each step within `timeout` (023 FR-023); the
/// DER answer. The future never fails: a failure is its `Err`.
pub fn fetch_on_shard(
    url: &str,
    id: &CertId,
    timeout: Duration,
) -> teseladb::engine::core::future::Future<Result<Vec<u8>, String>> {
    use teseladb::engine::core::coroutine::make_coroutine;
    let asked = ask_about(url, id);
    let url = url.to_owned();
    make_coroutine(async move {
        let (target, bytes) = match asked {
            Ok(a) => a,
            Err(e) => return Ok(Err(e)),
        };
        let to = crate::shard_client::Target {
            host: target.host,
            port: target.port,
            tls: None,
            connect: timeout,
            answer: timeout,
        };
        Ok(match crate::shard_client::exchange(to, bytes).await {
            Ok(r) => answer_of(&url, r.status, r.body),
            Err(e) => Err(format!("OCSP responder {url}: {e}")),
        })
    })
}

/// Asks `url` (HTTP) about `id`; the DER answer. From a thread that is not a shard.
pub fn fetch(url: &str, id: &CertId, timeout: Duration) -> Result<Vec<u8>, String> {
    let (url, id) = (url.to_owned(), id.clone());
    crate::shard_client::block_on(move || fetch_on_shard(&url, &id, timeout))
        .map_err(crate::error_text)?
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
    match question(leaf, issuer, now) {
        Err(r) => r,
        Ok((url, id)) => settle(ask(&url, &id), &id, issuer, now),
    }
}

/// What a round asks: the responder's URL and the certificate's id; or the round's outcome
/// when there is nothing to ask (no responder: never again; no id: a minute later).
pub fn question(
    leaf: &Certificate,
    issuer: &Certificate,
    now: i64,
) -> Result<(String, CertId), Round> {
    let Some(url) = ocsp::responder_urls(leaf).into_iter().next() else {
        return Err(Round { staple: None, next_secs: i64::MAX });
    };
    let id = CertId::new(leaf, issuer)
        .map_err(|_| Round { staple: None, next_secs: now + RETRY_SECS })?;
    Ok((url, id))
}

/// The outcome of a round whose responder answered `answer` about `id` at `now`.
pub fn settle(
    answer: Result<Vec<u8>, String>,
    id: &CertId,
    issuer: &Certificate,
    now: i64,
) -> Round {
    let retry = Round { staple: None, next_secs: now + RETRY_SECS };
    let answer = answer.and_then(|der| {
        ocsp::check(&der, id, issuer, now).map(|c| (der, c)).map_err(|e| e.to_string())
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
/// that names a responder; each new staple goes to the shards of the ports `certs` rotates.
pub fn spawn(
    port: TlsPort,
    issuer: Certificate,
    stop: fn() -> bool,
    certs: Option<std::sync::Arc<crate::certs::Certificates>>,
) {
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
                // A port of an engine that stopped serves nothing more.
                let _ = certs.as_ref().map(|c| c.publish());
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

/// The stapler as a task of shard 0 (023 FR-023): each round asks the responder with the engine,
/// puts the new staple in `port` and publishes it to every shard of each of `shards` before it
/// waits, on the engine's timers, for the next round.
pub mod on_shard {
    use std::cell::Cell;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::time::Duration;

    use teseladb::engine::core::condition_variable::ConditionVariable;
    use teseladb::engine::core::coroutine::make_coroutine;
    use teseladb::engine::core::future::Future;
    use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
    use teseladb_x509::Certificate;
    use teseladb_x509::ocsp;

    use super::{FETCH_TIMEOUT, RETRY_SECS, fetch_on_shard, question, settle, stapled};
    use crate::shard_tls::ShardTls;
    use crate::tls_port::TlsPort;

    /// A running stapler.
    pub struct Stapler {
        stop: Rc<Cell<bool>>,
        wake: Pin<Rc<ConditionVariable>>,
        done: Future<()>,
    }

    impl std::fmt::Debug for Stapler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Stapler").field("stopped", &self.stop.get()).finish()
        }
    }

    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    impl Stapler {
        /// Keeps `port`'s staple fresh, publishing it to `shards`; `None` when its
        /// certificate names no responder. `fetch_timeout` bounds each exchange.
        #[must_use]
        pub fn start(
            port: TlsPort,
            issuer: Certificate,
            shards: Vec<ShardTls>,
            fetch_timeout: Option<Duration>,
        ) -> Option<Stapler> {
            let leaf = port.config().and_then(|c| c.identity.chain.first().cloned())?;
            if ocsp::responder_urls(&leaf).is_empty() {
                return None;
            }
            let stop = Rc::new(Cell::new(false));
            let wake = Rc::pin(ConditionVariable::new());
            let timeout = fetch_timeout.unwrap_or(FETCH_TIMEOUT);
            let (s, w) = (stop.clone(), wake.clone());
            let done = make_coroutine(async move {
                while !s.get() {
                    let now = now_secs();
                    let r = match question(&leaf, &issuer, now) {
                        Err(r) => r,
                        Ok((url, id)) => {
                            let answer = fetch_on_shard(&url, &id, timeout).await?;
                            settle(answer, &id, &issuer, now)
                        }
                    };
                    if let Some(next) = stapled(&port, r.staple) {
                        port.replace(&next);
                        for t in &shards {
                            t.publish_port(&port).await?;
                        }
                    }
                    let secs = (r.next_secs - now).clamp(1, RETRY_SECS * 60);
                    let nanos = i64::try_from(Duration::from_secs(secs.unsigned_abs()).as_nanos())
                        .unwrap_or(i64::MAX);
                    let until = SteadyTimePoint(SteadyClockType::now().0.saturating_add(nanos));
                    // A stop wakes it early; a timeout is the next round.
                    let _ = w.as_ref().wait_until::<SteadyClockType>(until).await;
                }
                Ok(())
            });
            Some(Stapler { stop, wake, done })
        }

        /// Stops it; resolves once its round in progress ended.
        pub fn stop(self) -> Future<()> {
            self.stop.set(true);
            self.wake.signal();
            self.done
        }
    }
}
