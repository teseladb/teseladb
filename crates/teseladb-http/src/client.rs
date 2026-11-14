//! What a client needs around the messages: the URL of a server (`http` or `https`, host,
//! port, path and query, RFC 3986 without user information or fragment) and the requests of
//! the STS and JWKS clients, each on its own connection (`Connection: close`).

use crate::http1::{HttpError, Request};

/// The scheme of a URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// Plain HTTP.
    Http,
    /// HTTP over TLS.
    Https,
}

impl Scheme {
    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// A server's URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    /// Scheme.
    pub scheme: Scheme,
    /// Host name or IP address, without brackets.
    pub host: String,
    /// Port (the scheme's own when the URL has none).
    pub port: u16,
    /// Path and query, at least `/`.
    pub path_and_query: String,
}

fn bad(what: &'static str) -> HttpError {
    HttpError::BadRequest(what)
}

impl Url {
    /// Reads `text`.
    ///
    /// # Errors
    ///
    /// [`HttpError::BadRequest`] for a URL that is not `http://` or `https://` with a host and a valid
    /// port, or that has a fragment or a character outside printable ASCII.
    pub fn parse(text: &str) -> Result<Url, HttpError> {
        let (scheme, rest) = match text.split_once("://") {
            Some(("http", r)) => (Scheme::Http, r),
            Some(("https", r)) => (Scheme::Https, r),
            _ => return Err(bad("URL scheme")),
        };
        if rest.contains('#') || !rest.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(bad("URL"));
        }
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let path = if path.starts_with('?') { format!("/{path}") } else { path.to_owned() };
        if authority.is_empty() || authority.contains('@') {
            return Err(bad("URL host"));
        }
        let (host, port) = match authority.strip_prefix('[') {
            Some(v6) => {
                let (host, after) = v6.split_once(']').ok_or(bad("URL host"))?;
                (host, after.strip_prefix(':'))
            }
            None => match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            },
        };
        let port = match port {
            None => scheme.default_port(),
            Some(p) => p.parse().map_err(|_| bad("URL port"))?,
        };
        if host.is_empty() {
            return Err(bad("URL host"));
        }
        Ok(Url { scheme, host: host.to_owned(), port, path_and_query: path })
    }

    /// `host[:port]` for the `Host` field: the port only when it is not the scheme's own, an IPv6
    /// address in brackets.
    #[must_use]
    pub fn authority(&self) -> String {
        let host =
            if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == self.scheme.default_port() { host } else { format!("{host}:{}", self.port) }
    }
}

fn base(method: &str, url: &Url, user_agent: &str) -> Request {
    Request::new(method, &url.path_and_query)
        .header("Host", &url.authority())
        .header("User-Agent", user_agent)
        .header("Accept", "application/json")
        .header("Connection", "close")
}

/// `GET` of `url`, from the client named `user_agent`.
#[must_use]
pub fn get(url: &Url, user_agent: &str) -> Request {
    base("GET", url, user_agent)
}

/// `POST` of `body` of type `content_type` to `url`, from the client named `user_agent`.
#[must_use]
pub fn post(url: &Url, user_agent: &str, content_type: &str, body: &[u8]) -> Request {
    base("POST", url, user_agent).header("Content-Type", content_type).body(body.to_vec())
}
