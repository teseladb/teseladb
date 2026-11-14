//! The administration port (007 FR-020, contract `admin-cli.md`): `GET /metrics`, `GET /healthz`
//! and the routes of `teseladb admin`, over HTTP/1.1 with one request per connection. The
//! requests are parsed and the answers encoded here without I/O; [`AdminListener`] owns the
//! socket. On a node with a PKI the port speaks TLS and names each [`Caller`] by its client
//! certificate (013 FR-017); a request without TLS only gets `/healthz`. The HTTP/1.1 messages
//! are read and written by `teseladb-http` (019 T008), the same code as the HTTP port's.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use teseladb_http::http1::{Limits, Request, ResponseReader};
use teseladb_http::server1::{Refused, RequestReader, encode_response};
use teseladb_tls::ClientConfig;
use teseladb_x509::{Certificate, Name, oid};

use crate::tls_port::TlsPort;
use crate::tls_stream::TlsStream;

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
    /// organizational unit and domain components of the node's own, RefDb's rule for members.
    Member,
    /// The REFDB-X509 user name of the client certificate, a user of `$external`.
    User(String),
}

/// What answers the port's requests.
pub type Handler = Arc<dyn Fn(&HttpRequest, &Caller) -> HttpResponse + Send + Sync>;

/// First byte of a TLS handshake record.
const TLS_HANDSHAKE: u8 = 22;

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

fn other(e: String) -> io::Error {
    io::Error::other(e)
}

/// The administration port's listener: one thread that takes a connection, reads its request,
/// answers it and closes it.
#[derive(Debug)]
pub struct AdminListener {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl AdminListener {
    /// Listens on `addr` and answers with `handler`, over TLS with `tls`.
    pub fn spawn(
        addr: SocketAddr,
        handler: Handler,
        tls: Option<AdminTls>,
    ) -> io::Result<AdminListener> {
        let listener = crate::listen::bind(crate::listen::ADMIN, addr)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new().name("teseladb-admin".into()).spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                // A connection that fails or times out only loses its own answer.
                let _ = stream.and_then(|s| serve(s, &handler, tls.as_ref()));
            }
        })?;
        Ok(AdminListener { addr, stop, thread })
    }

    /// The address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops listening.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        // The thread waits in `accept`: a connection wakes it, and it sees the stop.
        let _ = TcpStream::connect(self.addr);
        let _ = self.thread.join();
    }
}

/// Reads bytes from a plain socket: empty when the peer closed.
fn read_plain(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut chunk = vec![0u8; 4096];
    let n = stream.read(&mut chunk)?;
    chunk.truncate(n);
    Ok(chunk)
}

/// The answer to the request that `buf` and then `more` hold; `None` if the peer closed first.
fn answer(
    mut buf: Vec<u8>,
    mut more: impl FnMut() -> io::Result<Vec<u8>>,
    handler: &Handler,
    caller: &Caller,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        match parse_request(&buf) {
            Ok(Some(req)) => return Ok(Some(handler(&req, caller).encode())),
            Ok(None) => {}
            Err(e) => return Ok(Some(e.response().encode())),
        }
        let data = more()?;
        if data.is_empty() {
            return Ok(None);
        }
        buf.extend_from_slice(&data);
    }
}

/// Reads one request from `stream` and answers it: plain on a node without TLS; on one with
/// TLS, over TLS when the first byte opens a handshake and plain otherwise, as [`Caller::Anonymous`].
fn serve(mut stream: TcpStream, handler: &Handler, tls: Option<&AdminTls>) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let (first, caller) = match tls {
        None => (Vec::new(), Caller::Open),
        Some(tls) => {
            let first = read_plain(&mut stream)?;
            match (first.first(), tls.port.config()) {
                (Some(&TLS_HANDSHAKE), Some(config)) => {
                    let mut s = TlsStream::server(stream, config, &first).map_err(other)?;
                    let caller = tls.caller(s.peer_certificates());
                    if let Some(a) =
                        answer(Vec::new(), || s.recv().map_err(other), handler, &caller)?
                    {
                        s.send(&a).map_err(other)?;
                    }
                    s.close();
                    return Ok(());
                }
                _ => (first, Caller::Anonymous),
            }
        }
    };
    if let Some(a) = answer(first, || read_plain(&mut stream), handler, &caller)? {
        stream.write_all(&a)?;
        stream.flush()?;
    }
    Ok(())
}

/// Sends `req` to the administration port at `addr` and returns the answer's status and body.
pub fn request(
    addr: &str,
    method: Method,
    target: &str,
    body: &[u8],
) -> io::Result<(u16, Vec<u8>)> {
    request_with(addr, None, method, target, body)
}

/// The host of `addr` (`host:port` or `[v6]:port`).
fn host_of(addr: &str) -> &str {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// [`request`], over TLS with `tls`: the server's certificate has to name the host of `addr`.
pub fn request_with(
    addr: &str,
    tls: Option<Arc<ClientConfig>>,
    method: Method,
    target: &str,
    body: &[u8],
) -> io::Result<(u16, Vec<u8>)> {
    let s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(ANSWER_WAIT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    let verb = match method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    let bad = |e: teseladb_http::http1::HttpError| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))
    };
    let req = Request::new(verb, target)
        .header("Host", addr)
        .header("Connection", "close")
        .header("Content-Length", &body.len().to_string())
        .body(body.to_vec())
        .encode()
        .map_err(bad)?;
    let answer = match tls {
        None => {
            let mut s = s;
            s.write_all(&req)?;
            let mut answer = Vec::new();
            s.read_to_end(&mut answer)?;
            answer
        }
        Some(config) => {
            let mut t = TlsStream::client(s, config, host_of(addr)).map_err(other)?;
            t.send(&req).map_err(other)?;
            t.recv_to_end().map_err(other)?
        }
    };
    let mut reader = ResponseReader::new(Limits::default());
    let r = match reader.feed(&answer).map_err(bad)? {
        Some(r) => r,
        None => reader.finish().map_err(bad)?,
    };
    Ok((r.status, r.body))
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

    #[test]
    fn the_listener_answers_each_request_and_stops() {
        let handler: Handler = Arc::new(|r: &HttpRequest, c: &Caller| {
            assert_eq!(*c, Caller::Open);
            HttpResponse::text(200, &format!("{:?} {}", r.method, r.path))
        });
        let l = AdminListener::spawn("127.0.0.1:0".parse().unwrap(), handler, None).unwrap();
        let at = l.local_addr().to_string();
        assert_eq!(
            request(&at, Method::Get, "/healthz", b"").unwrap(),
            (200, b"Get /healthz".to_vec())
        );
        assert_eq!(
            request(&at, Method::Post, "/a?b=1", b"{}").unwrap(),
            (200, b"Post /a".to_vec())
        );
        // A request the port cannot parse gets its error.
        let mut s = TcpStream::connect(&at).unwrap();
        s.write_all(b"BREW /x HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 405 "), "{answer}");
        // A client that closes before sending a whole request gets nothing.
        drop(TcpStream::connect(&at).unwrap());
        assert_eq!(request(&at, Method::Get, "/x", b"").unwrap().0, 200);
        l.stop();
        assert!(request(&at, Method::Get, "/x", b"").is_err());
    }

    #[test]
    fn a_client_waits_for_an_answer_slower_than_the_connections_timeout() {
        // A handler may wait for a core and then for Group 0 (`range split`) longer than a
        // connection has to send its request: the client still gets the answer.
        let handler: Handler = Arc::new(|_: &HttpRequest, _: &Caller| {
            thread::sleep(IO_TIMEOUT + Duration::from_millis(300));
            HttpResponse::text(200, "late")
        });
        let l = AdminListener::spawn("127.0.0.1:0".parse().unwrap(), handler, None).unwrap();
        let at = l.local_addr().to_string();
        assert_eq!(request(&at, Method::Get, "/x", b"").unwrap(), (200, b"late".to_vec()));
        l.stop();
    }

    fn random(b: &mut [u8]) -> std::io::Result<()> {
        teseladb_sys::fill_random(b)
    }

    #[test]
    fn over_tls_each_caller_is_named_by_its_certificate() {
        use crate::pki;
        use teseladb_tls::{CertifiedKey, ClientAuth, ServerConfig, Trust, Version};
        use teseladb_x509::ext::GeneralName;
        let base = std::env::temp_dir().join(format!("teseladb-admin-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
        let params = pki::PkiParams { now, node_names: vec![GeneralName::Ip(vec![127, 0, 0, 1])] };
        let cluster = pki::load_or_create(&base.join("cluster"), &params, &mut random).unwrap();
        // Another CA, trusted for clients like a `--tlsCAFile`: its certificates are users.
        let users = pki::load_or_create(&base.join("users"), &params, &mut random).unwrap();
        let identity =
            |p: &pki::Pki| CertifiedKey { chain: p.node_chain.clone(), key: p.node_key.clone() };
        let mut server = ServerConfig::new(identity(&cluster));
        server.min_version = Version::Tls12;
        server.client_auth = ClientAuth::Optional;
        server.trust = Trust::anchors(vec![cluster.ca.clone(), users.ca.clone()]);
        let tls = AdminTls {
            port: TlsPort::new(crate::tls_port::TlsMode::RequireTls, Some(server)),
            ca: cluster.ca.clone(),
            node: cluster.node_chain[0].subject.clone(),
        };
        let handler: Handler =
            Arc::new(|_: &HttpRequest, c: &Caller| HttpResponse::text(200, &format!("{c:?}")));
        let tls_copy = tls.clone();
        let l = AdminListener::spawn("127.0.0.1:0".parse().unwrap(), handler, Some(tls)).unwrap();
        let at = l.local_addr().to_string();
        let client = |who: Option<&pki::Pki>| {
            let mut c = ClientConfig::new(Trust::anchors(vec![cluster.ca.clone()]));
            c.identity = who.map(identity);
            Some(Arc::new(c))
        };
        let ask = |tls| {
            let (status, body) = request_with(&at, tls, Method::Get, "/metrics", b"").unwrap();
            assert_eq!(status, 200);
            String::from_utf8(body).unwrap()
        };
        assert_eq!(ask(client(Some(&cluster))), "Member");
        let user = users.node_chain[0].subject.x509_user_name();
        assert_eq!(ask(client(Some(&users))), format!("{:?}", Caller::User(user)));
        // A client certificate of the cluster's CA with another organizational unit is a user.
        let key = teseladb_x509::PrivateKey::ed25519(&[9; 32]);
        let name = teseladb_x509::Name::build(&[
            (oid::ORGANIZATION, "TeselaDB"),
            (oid::ORGANIZATIONAL_UNIT, "operators"),
            (oid::COMMON_NAME, "ops"),
        ]);
        let p = teseladb_x509::generate::CertificateParams::node(
            [3; 16],
            name.clone(),
            now - 60,
            now + 600,
        );
        let cert =
            teseladb_x509::generate::issue(&p, &key.public_key(), &cluster.ca, &cluster.ca_key)
                .unwrap();
        let mut c = ClientConfig::new(Trust::anchors(vec![cluster.ca.clone()]));
        c.identity = Some(CertifiedKey { chain: vec![cert], key });
        assert_eq!(ask(Some(Arc::new(c))), format!("{:?}", Caller::User(name.x509_user_name())));
        assert_eq!(ask(client(None)), "Anonymous");
        // Without TLS the request still gets an answer, as an anonymous caller.
        assert_eq!(ask(None), "Anonymous");
        // A server whose certificate does not name the host is refused.
        let wrong = format!("localhost:{}", l.local_addr().port());
        assert!(request_with(&wrong, client(None), Method::Get, "/", b"").is_err());
        // A handshake that fails and a peer that closes during it only lose their own answer.
        let mut s = TcpStream::connect(&at).unwrap();
        s.write_all(&[22, 3, 1, 0, 1, 0]).unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        let mut s = TcpStream::connect(&at).unwrap();
        s.write_all(&[22, 3, 1]).unwrap();
        drop(s);
        assert_eq!(ask(client(None)), "Anonymous");
        assert_eq!(host_of("[::1]:5"), "::1");
        assert_eq!(host_of("h"), "h");
        // A certificate of another CA with the cluster CA's name is not a node.
        let impostor = pki::load_or_create(&base.join("impostor"), &params, &mut random).unwrap();
        let same_name = AdminTls { ca: impostor.ca.clone(), ..tls_copy.clone() };
        assert_eq!(
            same_name.caller(&cluster.node_chain),
            Caller::User(cluster.node_chain[0].subject.x509_user_name())
        );
        assert_eq!(tls_copy.caller(&cluster.node_chain), Caller::Member);
        // A certificate of an issuer with another name is a user.
        let other_key = teseladb_x509::PrivateKey::ed25519(&[4; 32]);
        let other_name =
            teseladb_x509::Name::build(&[(teseladb_x509::oid::COMMON_NAME, "elsewhere")]);
        let other = teseladb_x509::generate::self_signed(
            &teseladb_x509::generate::CertificateParams::node([6; 16], other_name, 0, i64::MAX / 2),
            &other_key,
        )
        .unwrap();
        assert_eq!(
            tls_copy.caller(std::slice::from_ref(&other)),
            Caller::User(other.subject.x509_user_name())
        );
        // A certificate that names the cluster's CA as issuer but another key signed: a user.
        let mut forged_params = teseladb_x509::generate::CertificateParams::node(
            [7; 16],
            cluster.ca.subject.clone(),
            0,
            i64::MAX / 2,
        );
        forged_params.ca = Some(None);
        let forged_ca = teseladb_x509::generate::self_signed(&forged_params, &other_key).unwrap();
        let leaf_params = teseladb_x509::generate::CertificateParams::node(
            [8; 16],
            cluster.node_chain[0].subject.clone(),
            0,
            i64::MAX / 2,
        );
        let forged = teseladb_x509::generate::issue(
            &leaf_params,
            &cluster.node_key.public_key(),
            &forged_ca,
            &other_key,
        )
        .unwrap();
        assert_eq!(
            tls_copy.caller(std::slice::from_ref(&forged)),
            Caller::User(forged.subject.x509_user_name())
        );
        // A TLS client that leaves before asking gets no answer.
        let sock = std::net::TcpStream::connect(&at).unwrap();
        let s = crate::tls_stream::TlsStream::client(sock, client(None).unwrap(), "127.0.0.1");
        drop(s);
        // A port whose TLS is disabled reads a handshake as a plain request, which it is not.
        let plain_tls = AdminTls {
            port: TlsPort::new(crate::tls_port::TlsMode::Disabled, None),
            ..tls_copy.clone()
        };
        let handler: Handler =
            Arc::new(|_: &HttpRequest, c: &Caller| HttpResponse::text(200, &format!("{c:?}")));
        let p =
            AdminListener::spawn("127.0.0.1:0".parse().unwrap(), handler, Some(plain_tls)).unwrap();
        let p_at = p.local_addr().to_string();
        assert_eq!(request(&p_at, Method::Get, "/", b"").unwrap().1, b"Anonymous");
        assert!(request_with(&p_at, client(None), Method::Get, "/", b"").is_err());
        p.stop();
        // An answer that is not HTTP.
        let raw = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let raw_at = raw.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for (i, mut s) in raw.incoming().flatten().enumerate() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let answer: &[u8] = if i == 0 { b"garbage" } else { b"\xff\xfe\r\n\r\n" };
                let _ = s.write_all(answer);
            }
        });
        for _ in 0..2 {
            let e = request(&raw_at, Method::Get, "/", b"").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
        l.stop();
        let _ = std::fs::remove_dir_all(&base);
    }
}
