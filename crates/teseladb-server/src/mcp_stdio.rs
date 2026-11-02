//! `teseladb mcp --uri https://<node>:27080` (019 FR-020, contract `mcp-server.md`): MCP over
//! stdio for clients that launch their servers, relaying each JSON-RPC message of standard input
//! to `POST /mcp` of a node and writing its answers to standard output, one per line. It keeps
//! the `Mcp-Session-Id` the node gives to `initialize` and closes the session with `DELETE` at the
//! end of the input. The credential is the API key of `TESELADB_API_KEY` or the client
//! certificate of `--tlsCertificateKeyFile`; none goes in the arguments.

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::sse;
use teseladb_tls::ClientConfig;

use crate::admin::AdminCredentials;
use crate::tls_stream::TlsStream;

/// The variable that holds the API key.
pub const API_KEY_VAR: &str = "TESELADB_API_KEY";
/// The path of MCP on the HTTP port.
pub const MCP_PATH: &str = "/mcp";
/// The field of the session.
pub const SESSION_HEADER: &str = "Mcp-Session-Id";
/// The field of the revision.
pub const VERSION_HEADER: &str = "MCP-Protocol-Version";
/// How long an answer may take.
const ANSWER_WAIT: Duration = Duration::from_secs(300);

/// Options of `teseladb mcp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOpts {
    /// `--uri`: `https://host:port` (or `http://` for a node started with `--insecure`).
    pub uri: String,
    /// `--tlsCAFile`.
    pub ca_file: Option<PathBuf>,
    /// `--tlsCertificateKeyFile`.
    pub certificate_key_file: Option<PathBuf>,
}

/// Usage of `teseladb mcp`.
pub const MCP_USAGE: &str = "usage: teseladb mcp --uri https://<node>:<port> [--tlsCAFile <pem>] [--tlsCertificateKeyFile <pem>]\n  the API key goes in TESELADB_API_KEY";

/// Parses the arguments after `mcp`.
pub fn parse(args: &[String]) -> Result<McpOpts, String> {
    let mut o = McpOpts { uri: String::new(), ca_file: None, certificate_key_file: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let value = || it.clone().next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--uri" => o.uri = value()?,
            "--tlsCAFile" => o.ca_file = Some(value()?.into()),
            "--tlsCertificateKeyFile" => o.certificate_key_file = Some(value()?.into()),
            other => return Err(format!("unknown argument {other}\n{MCP_USAGE}")),
        }
        it.next();
    }
    if o.uri.is_empty() {
        return Err(MCP_USAGE.to_owned());
    }
    endpoint(&o.uri)?;
    Ok(o)
}

/// `host:port` of `uri` and whether it is TLS.
fn endpoint(uri: &str) -> Result<(String, bool), String> {
    let (rest, tls) = match (uri.strip_prefix("https://"), uri.strip_prefix("http://")) {
        (Some(r), _) => (r, true),
        (None, Some(r)) => (r, false),
        _ => return Err(format!("--uri must start with https:// or http://, not {uri}")),
    };
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("--uri {uri} names no host"));
    }
    let with_port = if host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        host.to_owned()
    } else {
        format!("{host}:27080")
    };
    Ok((with_port, tls))
}

/// The relay: where it sends and with what.
struct Relay {
    addr: String,
    tls: Option<Arc<ClientConfig>>,
    key: Option<String>,
    session: Option<String>,
    version: Option<String>,
}

impl Relay {
    fn exchange(&self, req: Request) -> Result<Response, String> {
        let mut req = req.header("Host", &self.addr).header("Connection", "close");
        if let Some(k) = &self.key {
            req = req.header("Authorization", &format!("Bearer {k}"));
        }
        if let Some(s) = &self.session {
            req = req.header(SESSION_HEADER, s);
        }
        if let Some(v) = &self.version {
            req = req.header(VERSION_HEADER, v);
        }
        let bytes = req.encode().map_err(debugged)?;
        let s = TcpStream::connect(&self.addr).map_err(|e| format!("{}: {e}", self.addr))?;
        s.set_read_timeout(Some(ANSWER_WAIT)).map_err(shown)?;
        let answer = match &self.tls {
            None => {
                let mut s = s;
                s.write_all(&bytes).map_err(shown)?;
                let mut out = Vec::new();
                s.read_to_end(&mut out).map_err(shown)?;
                out
            }
            Some(config) => {
                let host = self.addr.rsplit_once(':').map_or(self.addr.as_str(), |(h, _)| h);
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let mut t = TlsStream::client(s, config.clone(), host)?;
                t.send(&bytes)?;
                t.recv_to_end()?
            }
        };
        let mut reader = ResponseReader::new(Limits::default());
        match reader.feed(&answer).map_err(debugged)? {
            Some(r) => Ok(r),
            None => reader.finish().map_err(debugged),
        }
    }

    /// Sends `line` and writes the messages of the answer to `out`.
    fn relay(&mut self, line: &str, out: &mut dyn Write) -> Result<(), String> {
        let req = Request::new("POST", MCP_PATH)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(line.as_bytes().to_vec());
        let r = self.exchange(req)?;
        if matches!(r.status, 401 | 403) {
            return Err(format!("the node refused the credential ({})", r.status));
        }
        if let Some(s) = r.header(SESSION_HEADER) {
            self.session = Some(s.to_owned());
        }
        if self.version.is_none() {
            self.version = negotiated(&r.body);
        }
        let event_stream =
            r.header("Content-Type").is_some_and(|t| t.starts_with(sse::EVENT_STREAM));
        if event_stream {
            for e in sse::Reader::default().feed(&r.body) {
                writeln!(out, "{}", e.data).map_err(shown)?;
            }
        } else if !r.body.is_empty() {
            let text = String::from_utf8_lossy(&r.body);
            writeln!(out, "{}", text.trim_end()).map_err(shown)?;
        } else if r.status >= 400 {
            return Err(format!("the node answered {}", r.status));
        }
        out.flush().map_err(shown)
    }
}

/// The message of an error that displays.
fn shown(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The message of an error that only debugs.
fn debugged(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

/// The `protocolVersion` of an `initialize` result, which the requests after it carry.
fn negotiated(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let v = teseladb_json::parse(text.as_bytes()).ok()?;
    let r = v.as_object()?.get("result")?.as_object()?;
    r.get("protocolVersion")?.as_str().map(str::to_owned)
}

/// Relays standard input to the node of `o` with `key`: 0 at the end of the input, 1 when the
/// node cannot be reached or the TLS options are wrong.
pub fn run(
    o: &McpOpts,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
    key: Option<String>,
) -> i32 {
    let (addr, tls) = match endpoint(&o.uri) {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(err, "teseladb mcp: {e}");
            return 1;
        }
    };
    let config = if tls {
        let creds = AdminCredentials {
            dbpath: PathBuf::from("/nonexistent/teseladb-mcp"),
            certificate_key_file: o.certificate_key_file.clone(),
            ca_file: o.ca_file.clone(),
        };
        match (creds.client(), &o.ca_file) {
            (Ok(Some(c)), _) => Some(c),
            (Ok(None), Some(ca)) => match ca_only(ca) {
                Ok(c) => Some(c),
                Err(e) => {
                    let _ = writeln!(err, "teseladb mcp: {e}");
                    return 1;
                }
            },
            (Ok(None), None) => {
                let _ = writeln!(err, "teseladb mcp: an https:// URI needs --tlsCAFile");
                return 1;
            }
            (Err(e), _) => {
                let _ = writeln!(err, "teseladb mcp: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let mut relay = Relay { addr, tls: config, key, session: None, version: None };
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                let _ = writeln!(err, "teseladb mcp: {e}");
                return 1;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Err(e) = relay.relay(line.trim_end(), out) {
            let _ = writeln!(err, "teseladb mcp: {e}");
            return 1;
        }
    }
    if relay.session.is_some() {
        let _ = relay.exchange(Request::new("DELETE", MCP_PATH));
    }
    0
}

/// A client configuration that trusts the CA of `ca` and presents no certificate.
fn ca_only(ca: &std::path::Path) -> Result<Arc<ClientConfig>, String> {
    let text =
        std::fs::read_to_string(ca).map_err(|e| format!("--tlsCAFile {}: {e}", ca.display()))?;
    let anchors = teseladb_x509::Certificate::from_pem_all(&text)
        .map_err(|e| format!("--tlsCAFile {}: {e}", ca.display()))?;
    // A file without a certificate would trust nothing, and every handshake would fail later
    // with an unknown issuer instead of naming the option.
    if anchors.is_empty() {
        return Err(format!("--tlsCAFile {}: no certificate in the file", ca.display()));
    }
    Ok(Arc::new(ClientConfig::new(teseladb_tls::Trust::anchors(anchors))))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_arguments_name_the_node_and_the_tls_files_only() {
        let o =
            parse(&args(&["--uri", "https://db.example:9000", "--tlsCAFile", "ca.pem"])).unwrap();
        assert_eq!(o.ca_file, Some(PathBuf::from("ca.pem")));
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["--uri"])).is_err());
        assert!(parse(&args(&["--uri", "ftp://x"])).is_err());
        assert!(parse(&args(&["--uri", "https://"])).is_err());
        assert!(parse(&args(&["--uri", "http://h", "--apiKey", "k"])).is_err());
        assert_eq!(endpoint("https://h").unwrap(), ("h:27080".to_owned(), true));
        assert_eq!(endpoint("http://h:1/mcp").unwrap(), ("h:1".to_owned(), false));
        assert_eq!(
            negotiated(br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}"#),
            Some("2025-06-18".to_owned())
        );
        assert_eq!(negotiated(b"[]"), None);
    }

    /// A node that answers one request with `answer` and closes: its `--uri`.
    fn canned(answer: &'static [u8]) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            // The whole request is read, so that closing does not reset the connection.
            let (mut seen, mut buf) = (Vec::new(), [0u8; 4096]);
            let whole = |seen: &[u8]| {
                let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") else {
                    return false;
                };
                let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .map_or(0, |n| n.trim().parse::<usize>().unwrap());
                seen.len() >= end + 4 + length
            };
            while !whole(&seen) {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    return;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            s.write_all(answer).unwrap();
        });
        format!("http://{at}")
    }

    /// `run` over `input` with `o`, writing to `out`: the code and what went to standard error.
    fn relay_with(o: &McpOpts, input: &[u8], out: &mut dyn Write) -> (i32, String) {
        let mut err = Vec::new();
        let code = run(o, &mut &input[..], out, &mut err, Some("k".to_owned()));
        (code, String::from_utf8(err).unwrap())
    }

    fn opts(uri: String) -> McpOpts {
        McpOpts { uri, ca_file: None, certificate_key_file: None }
    }

    /// A standard output that fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("closed"))
        }
    }

    const LINE: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";

    #[test]
    fn answers_the_relay_cannot_use_stop_it_with_a_message() {
        let failed =
            |answer: &'static [u8]| relay_with(&opts(canned(answer)), LINE, &mut Vec::new());
        let (code, err) =
            failed(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        assert_eq!((code, err.as_str()), (1, "teseladb mcp: the node answered 500\n"));
        let (code, err) = failed(b"not http at all\r\n\r\n");
        assert!(code == 1 && err.starts_with("teseladb mcp: "), "{err}");
        let (code, err) = failed(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc");
        assert!(code == 1 && err.starts_with("teseladb mcp: "), "{err}");
        // A standard output that fails stops the relay.
        let json = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(relay_with(&opts(canned(json)), LINE, &mut Broken).0, 1);
        let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 9\r\n\r\ndata: x\n\n";
        assert_eq!(relay_with(&opts(canned(sse)), LINE, &mut Broken).0, 1);
        // A node that does not listen.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let (code, err) = relay_with(&opts(format!("http://{closed}")), LINE, &mut Vec::new());
        assert!(code == 1 && err.contains(&closed.to_string()), "{err}");
        // Standard input that is not text.
        let (code, err) =
            relay_with(&opts("http://127.0.0.1:1".to_owned()), b"\xff\xfe\n", &mut Vec::new());
        assert!(code == 1 && err.starts_with("teseladb mcp: "), "{err}");
    }

    #[test]
    fn tls_files_that_do_not_load_stop_it_with_a_message() {
        let dir = std::env::temp_dir().join(format!("teseladb-mcp-stdio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad_ca = dir.join("bad-ca.pem");
        std::fs::write(&bad_ca, "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
            .unwrap();
        let o = McpOpts {
            uri: "https://127.0.0.1:1".to_owned(),
            ca_file: Some(bad_ca),
            certificate_key_file: None,
        };
        let (code, err) = relay_with(&o, LINE, &mut Vec::new());
        assert!(code == 1 && err.contains("--tlsCAFile"), "{err}");
        let o = McpOpts { certificate_key_file: Some(dir.join("missing.pem")), ..o };
        let (code, err) = relay_with(&o, LINE, &mut Vec::new());
        assert!(code == 1 && err.contains("--tlsCertificateKeyFile"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
