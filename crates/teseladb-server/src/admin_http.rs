//! The administration port (007 FR-020, contract `admin-cli.md`): `GET /metrics`, `GET /healthz`
//! and the routes of `teseladb admin`, over HTTP/1.1 with one request per connection. The
//! requests are parsed and the answers encoded here without I/O; the port runs on the engine's
//! shards ([`shard`], 023 FR-022). On a node with a PKI the port speaks TLS and names each [`Caller`] by its client
//! certificate (013 FR-017); a request without TLS only gets `/healthz`. The HTTP/1.1 messages
//! are read and written by `teseladb-http` (019 T008), the same code as the HTTP port's.

use std::sync::Arc;
use std::time::Duration;

use teseladb_http::http1::Limits;
use teseladb_http::server1::{Refused, RequestReader, encode_response};
use teseladb_x509::{Certificate, Name, oid};

use crate::tls_port::TlsPort;

/// The port and its client on the engine's shards (023 FR-022).
pub mod shard;
pub use shard::{request, request_with};

/// Bytes of a request line and headers at most.
pub const MAX_HEAD: usize = 16 * 1024;
/// Bytes of a request body at most.
pub const MAX_BODY: usize = 1024 * 1024;
/// How long a connection may take to send its request or read its answer.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client waits for the answer once its request is sent: longer than the longest
/// a handler waits (`range split` reads the catalog from a core, then commits in Group 0), so that
/// a slow answer still arrives and is not taken for an unreachable node.
pub const ANSWER_WAIT: Duration = Duration::from_secs(30);

/// The methods the port serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
}

/// A request of the port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Path, percent-decoded, without the query.
    pub path: String,
    /// Query parameters in order, percent-decoded.
    pub query: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The first value of query parameter `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A request the port cannot take: its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// Malformed (400).
    BadRequest,
    /// A method the port does not serve (405).
    MethodNotAllowed,
    /// Headers or body over the limits (431 or 413).
    TooLarge {
        /// Status to answer.
        status: u16,
    },
}

impl HttpError {
    /// The answer to send.
    pub fn response(self) -> HttpResponse {
        let status = match self {
            HttpError::BadRequest => 400,
            HttpError::MethodNotAllowed => 405,
            HttpError::TooLarge { status } => status,
        };
        HttpResponse::text(status, "")
    }
}

/// Media type of the Prometheus text exposition format.
pub const PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";
/// Media type of plain text.
pub const TEXT: &str = "text/plain; charset=utf-8";
/// Media type of JSON.
pub const JSON: &str = "application/json";

/// An answer of the port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: &'static str,
    /// Body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A plain-text answer.
    pub fn text(status: u16, body: &str) -> HttpResponse {
        HttpResponse { status, content_type: TEXT, body: body.as_bytes().to_vec() }
    }

    /// The answer as bytes: status line, `Content-Type`, `Content-Length`, `Connection: close`.
    pub fn encode(&self) -> Vec<u8> {
        let headers = [
            ("Content-Type".to_owned(), self.content_type.to_owned()),
            ("Connection".to_owned(), "close".to_owned()),
        ];
        encode_response(self.status, &headers, &self.body)
    }
}

/// Parses a request from the start of `buf` with the HTTP/1.1 reader of `teseladb-http`:
/// `Ok(None)` while it is incomplete.
pub fn parse_request(buf: &[u8]) -> Result<Option<HttpRequest>, HttpError> {
    let limits = Limits { max_head: MAX_HEAD, max_body: MAX_BODY };
    let incoming = match RequestReader::new(limits).feed(buf) {
        Ok(Some(i)) => i,
        Ok(None) => return Ok(None),
        Err(Refused { status: status @ (413 | 431), .. }) => {
            return Err(HttpError::TooLarge { status });
        }
        Err(_) => return Err(HttpError::BadRequest),
    };
    let r = incoming.request;
    let method = match r.method.as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => return Err(HttpError::MethodNotAllowed),
    };
    let (path, query) = r.target.split_once('?').unwrap_or((&r.target, ""));
    // The port takes the origin form only (RFC 9112, section 3.2.1).
    if !path.starts_with('/') {
        return Err(HttpError::BadRequest);
    }
    let path = decode(path)?;
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Ok((decode(k)?, decode(v)?))
        })
        .collect::<Result<Vec<_>, HttpError>>()?;
    Ok(Some(HttpRequest { method, path, query, body: r.body }))
}

/// Percent-decodes `text`, with `+` as a space (`application/x-www-form-urlencoded`).
fn decode(text: &str) -> Result<String, HttpError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).ok_or(HttpError::BadRequest)?;
                let hex = std::str::from_utf8(hex).map_err(|_| HttpError::BadRequest)?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| HttpError::BadRequest)?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| HttpError::BadRequest)
}

/// Who sent a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// Anyone: the node has no PKI (`--insecure`), as its clients' port.
    Open,
    /// No client certificate, or no TLS.
    Anonymous,
    /// A node of the cluster: a certificate of the cluster's CA with the organization,
    /// organizational unit and domain components of the node's own, the reference server's rule for
    /// members.
    Member,
    /// The X.509 authentication user name of the client certificate, a user of `$external`.
    User(String),
}

/// What answers the port's requests.
pub type Handler = Arc<dyn Fn(&HttpRequest, &Caller) -> HttpResponse + Send + Sync>;

/// TLS on the administration port: the server side (rotated with the node's certificate), the
/// cluster's CA and the subject of the node's certificate, which tell the nodes apart.
#[derive(Clone, Debug)]
pub struct AdminTls {
    /// The server side; client certificates are optional.
    pub port: TlsPort,
    /// The cluster's CA.
    pub ca: Certificate,
    /// The subject of the node's certificate.
    pub node: Name,
}

/// The attributes a member's subject shares with the node's.
const MEMBER_ATTRIBUTES: [&[u8]; 3] =
    [oid::ORGANIZATION, oid::ORGANIZATIONAL_UNIT, oid::DOMAIN_COMPONENT];

impl AdminTls {
    /// Who sent the client certificate chain `chain`, already validated by the handshake.
    pub fn caller(&self, chain: &[Certificate]) -> Caller {
        match chain.first() {
            None => Caller::Anonymous,
            Some(leaf)
                if leaf.issuer == self.ca.subject
                    && leaf.verify_signed_by(&self.ca.public_key).is_ok()
                    && MEMBER_ATTRIBUTES
                        .iter()
                        .all(|a| leaf.subject.values(a) == self.node.values(a)) =>
            {
                Caller::Member
            }
            Some(leaf) => Caller::User(leaf.subject.x509_user_name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Option<HttpRequest>, HttpError> {
        parse_request(text.as_bytes())
    }

    #[test]
    fn a_request_is_parsed_once_whole() {
        assert_eq!(parse("GET /metrics HTTP/1.1\r\nHost: x\r\n"), Ok(None));
        let r = parse("GET /admin/ranges?collection=db.c%2Fx&x=1 HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!((r.method, r.path.as_str()), (Method::Get, "/admin/ranges"));
        assert_eq!(r.param("collection"), Some("db.c/x"));
        assert_eq!(r.param("x"), Some("1"));
        assert_eq!(r.param("y"), None);
        let post = "POST /admin/config/a+b HTTP/1.1\r\nHost: x\r\ncontent-length: 4\r\n\r\n";
        assert_eq!(parse(post), Ok(None), "the body is not here yet");
        let r = parse(&format!("{post}{{}}ab")).unwrap().unwrap();
        assert_eq!(
            (r.method, r.path.as_str(), r.body.as_slice()),
            (Method::Post, "/admin/config/a b", &b"{}ab"[..])
        );
    }

    #[test]
    fn malformed_and_oversized_requests_are_refused() {
        assert_eq!(parse("GET\r\n\r\n"), Err(HttpError::BadRequest));
        assert_eq!(parse("GET /x HTTP/2\r\nHost: x\r\n\r\n"), Err(HttpError::BadRequest));
        assert_eq!(parse("GET x HTTP/1.1\r\nHost: x\r\n\r\n"), Err(HttpError::BadRequest));
        assert_eq!(parse("GET /%zz HTTP/1.1\r\nHost: x\r\n\r\n"), Err(HttpError::BadRequest));
        assert_eq!(
            parse("GET /x HTTP/1.1\r\nHost: x\r\nno colon\r\n\r\n"),
            Err(HttpError::BadRequest)
        );
        assert_eq!(
            parse("POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: z\r\n\r\n"),
            Err(HttpError::BadRequest)
        );
        // A body in chunks is read whole (RFC 9112, section 7.1), and a request without `Host`
        // is refused (section 3.2).
        let chunked =
            "POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert_eq!(parse(chunked).unwrap().unwrap().body, b"{}");
        assert_eq!(parse("GET /x HTTP/1.1\r\n\r\n"), Err(HttpError::BadRequest));
        assert_eq!(parse("PUT /x HTTP/1.1\r\nHost: x\r\n\r\n"), Err(HttpError::MethodNotAllowed));
        let long = format!("GET /{} HTTP/1.1\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(parse(&long), Err(HttpError::TooLarge { status: 431 }));
        // A head over the limit that already ended.
        let ended = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(parse(&ended), Err(HttpError::TooLarge { status: 431 }));
        let big =
            format!("POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(parse(&big), Err(HttpError::TooLarge { status: 413 }));
        assert_eq!(HttpError::MethodNotAllowed.response().status, 405);
        assert_eq!(HttpError::TooLarge { status: 413 }.response().status, 413);
        assert_eq!(HttpError::BadRequest.response().status, 400);
    }

    #[test]
    fn an_answer_has_its_length_and_closes_the_connection() {
        let r = HttpResponse { status: 503, content_type: TEXT, body: b"down".to_vec() };
        let text = String::from_utf8(r.encode()).unwrap();
        assert!(text.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{text}");
        assert!(text.contains("\r\nContent-Length: 4\r\n"));
        assert!(text.contains("\r\nConnection: close\r\n"));
        assert!(text.ends_with("\r\n\r\ndown"));
        for (status, reason) in [
            (200, "OK"),
            (400, "Bad Request"),
            (404, "Not Found"),
            (413, "Content Too Large"),
            (500, "Internal Server Error"),
            (299, ""),
        ] {
            let head = String::from_utf8(HttpResponse::text(status, "").encode()).unwrap();
            assert!(head.starts_with(&format!("HTTP/1.1 {status} {reason}\r\n")), "{head}");
        }
    }
}
