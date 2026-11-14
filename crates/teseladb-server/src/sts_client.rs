//! The STS of AWS IAM authentication (013 FR-025): `--setParameter awsSTSUrl`
//! (`https://sts.amazonaws.com` by default, HTTPS only, as the reference server checks it) and the
//! `GetCallerIdentity` request a login sends there with the client's signature.

use std::time::Duration;

use teseladb_auth::aws::{self, STS_BODY, STS_CONTENT_TYPE, StsRequest};
use teseladb_http::client::{self, Url};

use crate::https::{Https, system_trust};

/// How long each step of a request to the STS may take.
const STS_TIMEOUT: Duration = Duration::from_secs(10);

/// The node's STS.
#[derive(Clone, Debug)]
pub struct Sts {
    /// `awsSTSUrl`.
    pub url: String,
    target: Url,
    https: Https,
}

impl PartialEq for Sts {
    fn eq(&self, other: &Sts) -> bool {
        self.url == other.url
    }
}

impl Eq for Sts {}

impl Sts {
    /// The STS at `url` (the default without one), trusting the system's authorities; `fips`
    /// keeps only FIPS-approved algorithms on the TLS (`--tlsFIPSMode`).
    pub fn from_url(url: Option<&str>, fips: bool) -> Result<Sts, String> {
        let url = url.unwrap_or(aws::DEFAULT_STS_URL);
        Sts::with_https(url, Https::new(system_trust()?, STS_TIMEOUT).fips(fips))
    }

    /// The STS at `url` reached with `https`.
    pub fn with_https(url: &str, https: Https) -> Result<Sts, String> {
        aws::sts_host(url).map_err(|e| e.0)?;
        let target = Url::parse(url).map_err(|e| format!("awsSTSUrl {url}: {e:?}"))?;
        Ok(Sts { url: url.to_owned(), target, https })
    }

    /// Sends `req`: the answer's status and body.
    pub fn send(&self, req: &StsRequest) -> Result<(u16, Vec<u8>), String> {
        let mut http =
            client::post(&self.target, crate::USER_AGENT, STS_CONTENT_TYPE, STS_BODY.as_bytes());
        for (name, value) in &req.headers {
            http = http.header(name, value);
        }
        let answer = self.https.send(&self.target, &http)?;
        Ok((answer.status, answer.body))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::tls_stream::TlsStream;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use teseladb_tls::{CertifiedKey, ServerConfig, Trust};
    use teseladb_x509::ext::GeneralName;

    #[test]
    fn a_request_reaches_the_sts_with_the_clients_headers() {
        let dir = std::env::temp_dir().join(format!("teseladb-sts-{}", std::process::id()));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut s = TlsStream::server(sock, config, &[]).unwrap();
            let mut got = Vec::new();
            while !String::from_utf8_lossy(&got).ends_with(STS_BODY) {
                got.extend_from_slice(&s.recv().unwrap());
            }
            record.lock().unwrap().extend_from_slice(&got);
            let body = "<GetCallerIdentityResponse><GetCallerIdentityResult><Arn>arn:x</Arn></GetCallerIdentityResult></GetCallerIdentityResponse>";
            let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            s.send(answer.as_bytes()).unwrap();
            s.close();
        });
        let https = Https::new(Trust::anchors(vec![pki.ca.clone()]), STS_TIMEOUT);
        let sts = Sts::with_https(&format!("https://localhost:{port}"), https.clone()).unwrap();
        assert_eq!(sts, sts.clone());
        let req = StsRequest {
            headers: vec![(teseladb_names::AWS_HEADER_CHANNEL_BINDING.into(), "n".into())],
            body: STS_BODY,
        };
        let (status, body) = sts.send(&req).unwrap();
        assert_eq!(aws::caller(status, &body), Ok("arn:x".to_owned()));
        let text = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
        assert!(text.starts_with("POST / HTTP/1.1\r\n"), "{text}");
        let binding = format!("{}: n", teseladb_names::AWS_HEADER_CHANNEL_BINDING);
        for line in [
            binding.as_str(),
            "Content-Type: application/x-www-form-urlencoded",
            "Content-Length: 43",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        // Only HTTPS, as the reference server, and a URL that does not parse.
        assert_eq!(
            Sts::with_https("http://localhost:1", https.clone()).unwrap_err(),
            "STS URL must start with https://"
        );
        assert!(Sts::with_https("https://h:port", https).unwrap_err().contains("awsSTSUrl"));
        let default = Sts::from_url(None, false);
        assert!(default.is_err() || default.unwrap().url == aws::DEFAULT_STS_URL);
        let fips = Sts::from_url(None, true);
        assert!(
            fips.is_err() || fips.unwrap().https.tls.suites.iter().all(|s| s.is_fips_approved())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
