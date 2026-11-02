//! MCP revisions (contract `mcp-server.md`): 2026-07-28 without sessions, its version in the
//! `_meta` of each request and `server/discover`; and the revisions with `initialize` that
//! RefDb MCP Server 3.2.2 accepts, negotiated as it negotiates them.

use teseladb_json::{Number, Object, Value};

use crate::jsonrpc::{ErrorObject, HEADER_MISMATCH, INVALID_PARAMS, UNSUPPORTED_PROTOCOL_VERSION};

/// `_meta` key of the request's revision.
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key of the server's name and version in `server/discover`.
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// HTTP header with the request's revision.
pub const PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
/// How long a client may keep the reply of `server/discover`: one hour.
const DISCOVER_TTL_MS: i64 = 3_600_000;
/// The reply depends on who asks (the tools follow the roles).
const CACHE_SCOPE: &str = "private";
/// `resultType` of a complete result.
pub const RESULT_COMPLETE: &str = "complete";
/// The field of a 2026-07-28 result that says whether it is complete.
pub const RESULT_TYPE: &str = "resultType";

/// A protocol revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Revision {
    /// 2026-07-28: without sessions.
    R2026_07_28,
    /// 2025-11-25.
    R2025_11_25,
    /// 2025-06-18.
    R2025_06_18,
    /// 2025-03-26: with batches.
    R2025_03_26,
    /// 2024-11-05.
    R2024_11_05,
}

/// The newest revision with `initialize`: the answer to an unknown one.
pub const LATEST_SESSION_REVISION: Revision = Revision::R2025_11_25;

impl Revision {
    /// Every revision, newest first (the order of `supportedVersions`).
    pub const ALL: [Revision; 5] = [
        Revision::R2026_07_28,
        Revision::R2025_11_25,
        Revision::R2025_06_18,
        Revision::R2025_03_26,
        Revision::R2024_11_05,
    ];

    /// Its name.
    pub fn name(self) -> &'static str {
        match self {
            Revision::R2026_07_28 => "2026-07-28",
            Revision::R2025_11_25 => "2025-11-25",
            Revision::R2025_06_18 => "2025-06-18",
            Revision::R2025_03_26 => "2025-03-26",
            Revision::R2024_11_05 => "2024-11-05",
        }
    }

    /// The revision called `name`.
    pub fn from_name(name: &str) -> Option<Revision> {
        Revision::ALL.into_iter().find(|r| r.name() == name)
    }

    /// Whether it works without `initialize` and sessions.
    pub fn stateless(self) -> bool {
        self == Revision::R2026_07_28
    }

    /// Whether a body may carry a batch of messages.
    pub fn batches(self) -> bool {
        self == Revision::R2025_03_26
    }
}

fn supported() -> Value {
    Value::Array(Revision::ALL.iter().map(|r| Value::from(r.name())).collect())
}

/// The revision `initialize` answers to a client asking for `requested`: the same one if it has
/// sessions, the newest with sessions otherwise.
pub fn negotiate_initialize(requested: &str) -> Revision {
    Revision::from_name(requested).filter(|r| !r.stateless()).unwrap_or(LATEST_SESSION_REVISION)
}

fn invalid(message: &str) -> ErrorObject {
    ErrorObject::new(INVALID_PARAMS, format!("Invalid params: {message}"))
}

/// The revision of a request without session: the one in `params._meta`, which must match the
/// `MCP-Protocol-Version` header when the request came by HTTP (`header` is `None` on stdio).
pub fn request_revision(
    params: Option<&Object>,
    header: Option<&str>,
) -> Result<Revision, ErrorObject> {
    let missing = || invalid(&format!("_meta.{META_PROTOCOL_VERSION} is required"));
    let meta = match params.and_then(|p| p.get("_meta")) {
        None => return Err(missing()),
        Some(Value::Object(m)) => m,
        Some(_) => return Err(invalid("_meta must be an object")),
    };
    let version = match meta.get(META_PROTOCOL_VERSION) {
        None => return Err(missing()),
        Some(Value::String(v)) => v.as_str(),
        Some(_) => return Err(invalid(&format!("_meta.{META_PROTOCOL_VERSION} must be a string"))),
    };
    if let Some(h) = header.filter(|h| *h != version) {
        let msg = format!(
            "Header mismatch: {PROTOCOL_VERSION_HEADER} header value '{h}' does not match body value '{version}'"
        );
        return Err(ErrorObject::new(HEADER_MISMATCH, msg));
    }
    match Revision::from_name(version).filter(|r| r.stateless()) {
        Some(r) => Ok(r),
        None => {
            let mut data = Object::new();
            data.insert("supported", supported());
            data.insert("requested", version);
            let e = ErrorObject::new(UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version");
            Err(e.with_data(Value::Object(data)))
        }
    }
}

/// The result of `server/discover`.
pub fn discover_result(capabilities: Object, name: &str, version: &str) -> Object {
    let mut info = Object::new();
    info.insert("name", name);
    info.insert("version", version);
    let mut meta = Object::new();
    meta.insert(META_SERVER_INFO, Value::Object(info));
    let mut o = Object::new();
    o.insert(RESULT_TYPE, RESULT_COMPLETE);
    o.insert("supportedVersions", supported());
    o.insert("capabilities", Value::Object(capabilities));
    o.insert("_meta", Value::Object(meta));
    o.insert("ttlMs", Value::Number(Number::Int(DISCOVER_TTL_MS)));
    o.insert("cacheScope", CACHE_SCOPE);
    o
}
