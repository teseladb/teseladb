//! The cluster parameters of the HTTP port and of MCP (019 FR-003, FR-004, FR-023, FR-025;
//! DV-016), set with `setClusterParameter` like RefDb's own and read by every node from Group 0:
//!
//! - `httpOptions`: `cors.allowedOrigins`, the origins CORS lets in (none: CORS is off), and
//!   `rateLimits` with `concurrentRequests` (1,000) and `requestsPerSecond` (10,000) per
//!   credential, and `apiKeys.maxExpiresAfterDays` (365), the longest life of an API key;
//! - `mcpOptions`: `readOnly` (true), `allowDestructive` (false), `maxDocuments` (100),
//!   `maxBytes` (1 MiB), `maxTimeMS` (30 seconds) and `resourceUri` (none: the URI of `/mcp` at
//!   the `Host` of each request), the resource MCP's tokens are issued for (FR-027).
//!
//! A value given replaces the stored one; the fields it leaves out take their defaults.

use teseladb_auth::apikey::DEFAULT_MAX_EXPIRES_DAYS;
use teseladb_bson::{Document, RawDoc, Value};

use crate::error::{BAD_VALUE, Code, IDL_UNKNOWN_FIELD};
use crate::types::{number, wrong_type, wrong_types};

/// `httpOptions`.
pub const HTTP_OPTIONS: &str = "httpOptions";
/// `mcpOptions`.
pub const MCP_OPTIONS: &str = "mcpOptions";
/// Requests a credential may have running at once without a value.
pub const DEFAULT_CONCURRENT_REQUESTS: u64 = 1_000;
/// Requests a credential may start each second without a value.
pub const DEFAULT_REQUESTS_PER_SECOND: u64 = 10_000;
/// Documents a tool's result holds without a value.
pub const DEFAULT_MCP_MAX_DOCUMENTS: u64 = 100;
/// Bytes a tool's result holds without a value.
pub const DEFAULT_MCP_MAX_BYTES: u64 = 1 << 20;
/// `maxTimeMS` of a tool's operation without a value.
pub const DEFAULT_MCP_MAX_TIME_MS: u64 = 30_000;

const CORS: &str = "cors";
const ALLOWED_ORIGINS: &str = "allowedOrigins";
const RATE_LIMITS: &str = "rateLimits";
const CONCURRENT_REQUESTS: &str = "concurrentRequests";
const REQUESTS_PER_SECOND: &str = "requestsPerSecond";
const API_KEYS: &str = "apiKeys";
const MAX_EXPIRES_AFTER_DAYS: &str = "maxExpiresAfterDays";
const READ_ONLY: &str = "readOnly";
const ALLOW_DESTRUCTIVE: &str = "allowDestructive";
const MAX_DOCUMENTS: &str = "maxDocuments";
const MAX_BYTES: &str = "maxBytes";
const MAX_TIME_MS: &str = "maxTimeMS";
const RESOURCE_URI: &str = "resourceUri";
/// The structure of RefDb's errors of a cluster parameter.
const IDL: &str = "ClusterServerParameter";

/// What `httpOptions` holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpOptions {
    /// The origins CORS lets in; none turns it off.
    pub allowed_origins: Vec<String>,
    /// Requests a credential may have running at once.
    pub concurrent_requests: u64,
    /// Requests a credential may start each second.
    pub requests_per_second: u64,
    /// The longest life of an API key, in days (019 FR-016).
    pub api_key_max_days: u64,
}

/// What `mcpOptions` holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOptions {
    /// Only tools that read are listed and run.
    pub read_only: bool,
    /// Tools that destroy data are listed and run (and `read_only` is off).
    pub allow_destructive: bool,
    /// Documents a tool's result holds.
    pub max_documents: u64,
    /// Bytes a tool's result holds.
    pub max_bytes: u64,
    /// `maxTimeMS` of each tool's operation.
    pub max_time_ms: u64,
    /// The URI of the resource `/mcp` that tokens for MCP carry as audience (RFC 8707); `None`:
    /// the one at the `Host` of each request.
    pub resource_uri: Option<String>,
}

/// An error of a parameter: its code and message.
type ParseError = (Code, String);

fn unknown(path: &str) -> ParseError {
    (IDL_UNKNOWN_FIELD, format!("BSON field '{IDL}.{path}' is an unknown field."))
}

fn mistyped(e: crate::error::CommandError) -> ParseError {
    (e.code, e.errmsg)
}

/// `v` at `path`, a whole number of at least 1 (a double is truncated).
fn positive(path: &str, name: &str, v: &Value) -> Result<i64, ParseError> {
    let Some(x) = number(v) else {
        let expected = ["long", "int", "double", "decimal"];
        return Err(mistyped(wrong_types(&format!("{IDL}.{path}"), v, &expected)));
    };
    if x.trunc() < 1.0 {
        let why = format!("BSON field '{name}' value must be >= 1, actual value '{}'", x.trunc());
        return Err((BAD_VALUE, why));
    }
    Ok(x.trunc() as i64)
}

/// The sub-document `name` of a parameter.
fn object<'a>(name: &str, v: &'a Value) -> Result<&'a Document, ParseError> {
    match v {
        Value::Document(d) => Ok(d),
        other => Err(mistyped(wrong_type(&format!("{IDL}.{name}"), other, "object"))),
    }
}

/// Whether `s` is an origin of RFC 6454: `http` or `https`, a host and maybe a port, nothing
/// after.
fn is_origin(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) else {
        return false;
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || rest.starts_with('[') => (h, Some(p)),
        _ => (rest, None),
    };
    let host_ok = !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.[]:".contains(&b));
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.parse::<u16>().is_ok());
    host_ok && port_ok
}

fn origins(v: &Value) -> Result<Vec<Value>, ParseError> {
    let path = format!("{CORS}.{ALLOWED_ORIGINS}");
    let Value::Array(list) = v else {
        return Err(mistyped(wrong_type(&format!("{IDL}.{path}"), v, "array")));
    };
    let mut out = Vec::new();
    for (i, o) in list.iter().enumerate() {
        let Value::String(s) = o else {
            return Err(mistyped(wrong_type(&format!("{IDL}.{path}.{i}"), o, "string")));
        };
        if !is_origin(s) {
            let why = format!("'{s}' is not an origin: give each origin as scheme://host[:port]");
            return Err((BAD_VALUE, why));
        }
        out.push(o.clone());
    }
    Ok(out)
}

/// `httpOptions` without a value.
pub(crate) fn http_default() -> Document {
    Document::new()
        .with(CORS, Document::new().with(ALLOWED_ORIGINS, Value::Array(Vec::new())))
        .with(
            RATE_LIMITS,
            Document::new()
                .with(CONCURRENT_REQUESTS, DEFAULT_CONCURRENT_REQUESTS as i64)
                .with(REQUESTS_PER_SECOND, DEFAULT_REQUESTS_PER_SECOND as i64),
        )
        .with(API_KEYS, api_keys_doc(DEFAULT_MAX_EXPIRES_DAYS as i64))
}

fn api_keys_doc(max_days: i64) -> Document {
    Document::new().with(MAX_EXPIRES_AFTER_DAYS, max_days)
}

/// Reads a value of `httpOptions`.
pub(crate) fn http_parse(v: &Document) -> Result<Document, ParseError> {
    let mut allowed = Vec::new();
    let mut concurrent = DEFAULT_CONCURRENT_REQUESTS as i64;
    let mut per_second = DEFAULT_REQUESTS_PER_SECOND as i64;
    let mut max_days = DEFAULT_MAX_EXPIRES_DAYS as i64;
    for (k, x) in v.iter() {
        match k {
            CORS => {
                for (ck, cx) in object(CORS, x)?.iter() {
                    if ck != ALLOWED_ORIGINS {
                        return Err(unknown(&format!("{CORS}.{ck}")));
                    }
                    allowed = origins(cx)?;
                }
            }
            RATE_LIMITS => {
                for (rk, rx) in object(RATE_LIMITS, x)?.iter() {
                    let path = format!("{RATE_LIMITS}.{rk}");
                    match rk {
                        CONCURRENT_REQUESTS => concurrent = positive(&path, rk, rx)?,
                        REQUESTS_PER_SECOND => per_second = positive(&path, rk, rx)?,
                        _ => return Err(unknown(&path)),
                    }
                }
            }
            API_KEYS => {
                for (ak, ax) in object(API_KEYS, x)?.iter() {
                    let path = format!("{API_KEYS}.{ak}");
                    if ak != MAX_EXPIRES_AFTER_DAYS {
                        return Err(unknown(&path));
                    }
                    max_days = positive(&path, ak, ax)?;
                }
            }
            _ => return Err(unknown(k)),
        }
    }
    Ok(Document::new()
        .with(CORS, Document::new().with(ALLOWED_ORIGINS, Value::Array(allowed)))
        .with(
            RATE_LIMITS,
            Document::new()
                .with(CONCURRENT_REQUESTS, concurrent)
                .with(REQUESTS_PER_SECOND, per_second),
        )
        .with(API_KEYS, api_keys_doc(max_days)))
}

/// Whether `u` can name a protected resource: `http` or `https`, a host, and no fragment
/// (RFC 8707, 2).
fn is_resource_uri(u: &str) -> bool {
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"));
    rest.is_some_and(|r| {
        let host = r.split(['/', '?']).next().unwrap_or_default();
        !host.is_empty() && !u.contains('#') && !u.chars().any(char::is_whitespace)
    })
}

/// `mcpOptions` without a value.
pub(crate) fn mcp_default() -> Document {
    Document::new()
        .with(READ_ONLY, true)
        .with(ALLOW_DESTRUCTIVE, false)
        .with(MAX_DOCUMENTS, DEFAULT_MCP_MAX_DOCUMENTS as i64)
        .with(MAX_BYTES, DEFAULT_MCP_MAX_BYTES as i64)
        .with(MAX_TIME_MS, DEFAULT_MCP_MAX_TIME_MS as i64)
}

/// Reads a value of `mcpOptions`.
pub(crate) fn mcp_parse(v: &Document) -> Result<Document, ParseError> {
    let mut out = mcp_default();
    for (k, x) in v.iter() {
        let value = match k {
            READ_ONLY | ALLOW_DESTRUCTIVE => match x {
                Value::Boolean(b) => Value::Boolean(*b),
                other => return Err(mistyped(wrong_type(&format!("{IDL}.{k}"), other, "bool"))),
            },
            MAX_DOCUMENTS | MAX_BYTES | MAX_TIME_MS => Value::Int64(positive(k, k, x)?),
            RESOURCE_URI => match x {
                Value::String(u) if is_resource_uri(u) => Value::String(u.clone()),
                Value::String(u) => {
                    let m = format!(
                        "BSON field '{k}' is not an absolute http or https URI without a fragment: '{u}'"
                    );
                    return Err((BAD_VALUE, m));
                }
                other => return Err(mistyped(wrong_type(&format!("{IDL}.{k}"), other, "string"))),
            },
            _ => return Err(unknown(k)),
        };
        out.insert(k, value);
    }
    Ok(out)
}

fn stored_doc(stored: Option<&[u8]>) -> Document {
    stored.and_then(|b| RawDoc::from_bytes(b).ok()).map(Document::from).unwrap_or_default()
}

fn whole(d: &Document, k: &str, default: u64) -> u64 {
    match d.get(k) {
        Some(Value::Int64(n)) => u64::try_from(*n).unwrap_or(default),
        Some(Value::Int32(n)) => u64::try_from(*n).unwrap_or(default),
        _ => default,
    }
}

fn sub(d: &Document, k: &str) -> Document {
    match d.get(k) {
        Some(Value::Document(s)) => s.clone(),
        _ => Document::new(),
    }
}

/// `httpOptions` from its stored value (`None`: not set).
#[must_use]
pub fn http_options(stored: Option<&[u8]>) -> HttpOptions {
    let d = stored_doc(stored);
    let limits = sub(&d, RATE_LIMITS);
    let allowed_origins = match sub(&d, CORS).get(ALLOWED_ORIGINS) {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    HttpOptions {
        allowed_origins,
        concurrent_requests: whole(&limits, CONCURRENT_REQUESTS, DEFAULT_CONCURRENT_REQUESTS),
        requests_per_second: whole(&limits, REQUESTS_PER_SECOND, DEFAULT_REQUESTS_PER_SECOND),
        api_key_max_days: whole(
            &sub(&d, API_KEYS),
            MAX_EXPIRES_AFTER_DAYS,
            DEFAULT_MAX_EXPIRES_DAYS,
        ),
    }
}

/// `mcpOptions` from its stored value (`None`: not set).
#[must_use]
pub fn mcp_options(stored: Option<&[u8]>) -> McpOptions {
    let d = stored_doc(stored);
    let flag = |k: &str, default: bool| match d.get(k) {
        Some(Value::Boolean(b)) => *b,
        _ => default,
    };
    McpOptions {
        read_only: flag(READ_ONLY, true),
        allow_destructive: flag(ALLOW_DESTRUCTIVE, false),
        max_documents: whole(&d, MAX_DOCUMENTS, DEFAULT_MCP_MAX_DOCUMENTS),
        max_bytes: whole(&d, MAX_BYTES, DEFAULT_MCP_MAX_BYTES),
        max_time_ms: whole(&d, MAX_TIME_MS, DEFAULT_MCP_MAX_TIME_MS),
        resource_uri: match d.get(RESOURCE_URI) {
            Some(Value::String(u)) => Some(u.clone()),
            _ => None,
        },
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn origins_are_a_scheme_a_host_and_a_port() {
        for good in
            ["https://a.example", "http://localhost:3000", "https://[::1]:8443", "http://10.0.0.1"]
        {
            assert!(is_origin(good), "{good}");
        }
        for bad in [
            "*",
            "null",
            "ftp://a",
            "https://",
            "https://a/b",
            "https://a:",
            "https://a:99999",
            "https://a b",
        ] {
            assert!(!is_origin(bad), "{bad}");
        }
    }

    #[test]
    fn stored_values_of_another_shape_read_as_defaults() {
        let odd = Document::new()
            .with(CORS, Document::new().with(ALLOWED_ORIGINS, Value::Array(vec![Value::Int32(1)])))
            .with(MAX_DOCUMENTS, Value::Int32(4))
            .with(MAX_BYTES, "x");
        let bytes = odd.to_bytes().expect("bytes");
        assert!(http_options(Some(&bytes)).allowed_origins.is_empty());
        assert_eq!(mcp_options(Some(&bytes)).max_documents, 4);
        assert_eq!(mcp_options(Some(&bytes)).max_bytes, DEFAULT_MCP_MAX_BYTES);
        assert_eq!(http_options(Some(b"bad")), http_options(None));
    }
}
