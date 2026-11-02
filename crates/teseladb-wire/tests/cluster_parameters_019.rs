//! 019 T007 and T028: the cluster parameters of the HTTP port and of MCP (DV-016).
//! `httpOptions` holds the origins CORS lets in (none by default, so CORS is off, FR-004) and the
//! request limits per credential (FR-003); `mcpOptions` holds read-only mode, destructive tools
//! and the limits of a tool's result (FR-023, FR-025).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::http_parameters::{
    DEFAULT_CONCURRENT_REQUESTS, DEFAULT_MCP_MAX_BYTES, DEFAULT_MCP_MAX_DOCUMENTS,
    DEFAULT_MCP_MAX_TIME_MS, DEFAULT_REQUESTS_PER_SECOND, HTTP_OPTIONS, HttpOptions, MCP_OPTIONS,
    McpOptions, http_options, mcp_options,
};

fn set(name: &str, value: &str) -> Document {
    let value = teseladb_bson::extjson::parse(value).expect("a value");
    Document::new().with("setClusterParameter", Document::new().with(name, value))
}

fn stored(m: &Mem, name: &str) -> Option<Vec<u8>> {
    m.params.get(name).cloned()
}

#[test]
fn without_a_value_cors_is_off_and_mcp_is_read_only() {
    let mut m = Mem::new();
    assert_eq!(
        http_options(None),
        HttpOptions {
            allowed_origins: Vec::new(),
            concurrent_requests: DEFAULT_CONCURRENT_REQUESTS,
            requests_per_second: DEFAULT_REQUESTS_PER_SECOND,
        }
    );
    assert_eq!((DEFAULT_CONCURRENT_REQUESTS, DEFAULT_REQUESTS_PER_SECOND), (1_000, 10_000));
    assert_eq!(
        mcp_options(None),
        McpOptions {
            read_only: true,
            allow_destructive: false,
            max_documents: DEFAULT_MCP_MAX_DOCUMENTS,
            max_bytes: DEFAULT_MCP_MAX_BYTES,
            max_time_ms: DEFAULT_MCP_MAX_TIME_MS,
        }
    );
    assert_eq!(
        (DEFAULT_MCP_MAX_DOCUMENTS, DEFAULT_MCP_MAX_BYTES, DEFAULT_MCP_MAX_TIME_MS),
        (100, 1 << 20, 30_000)
    );
    let r = m.run("admin", doc(&[("getClusterParameter", Value::from(HTTP_OPTIONS))]));
    let Value::Array(list) = field(&r, "clusterParameters") else { panic!("{r:?}") };
    let Value::Document(p) = &list[0] else { panic!() };
    assert_eq!(p.get("_id"), Some(&Value::from(HTTP_OPTIONS)));
    assert_eq!(
        p.get("cors"),
        Some(&Value::Document(Document::new().with("allowedOrigins", Value::Array(Vec::new()))))
    );
    let r = m.run("admin", doc(&[("getClusterParameter", Value::from("*"))]));
    let Value::Array(list) = field(&r, "clusterParameters") else { panic!("{r:?}") };
    let names: Vec<Value> = list
        .iter()
        .filter_map(|v| match v {
            Value::Document(d) => d.get("_id").cloned(),
            _ => None,
        })
        .collect();
    assert!(
        names.contains(&Value::from(HTTP_OPTIONS)) && names.contains(&Value::from(MCP_OPTIONS))
    );
}

#[test]
fn allowed_origins_set_by_set_cluster_parameter_reach_the_http_port() {
    let mut m = Mem::new();
    let r = m.run(
        "admin",
        set(
            HTTP_OPTIONS,
            r#"{"cors":{"allowedOrigins":["https://app.example","http://localhost:3000"]}}"#,
        ),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let o = http_options(stored(&m, HTTP_OPTIONS).as_deref());
    assert_eq!(o.allowed_origins, ["https://app.example", "http://localhost:3000"]);
    // The limits not given keep their defaults.
    assert_eq!(o.concurrent_requests, DEFAULT_CONCURRENT_REQUESTS);
    let r = m.run(
        "admin",
        set(HTTP_OPTIONS, r#"{"rateLimits":{"concurrentRequests":5,"requestsPerSecond":7.9}}"#),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let o = http_options(stored(&m, HTTP_OPTIONS).as_deref());
    assert_eq!((o.allowed_origins.len(), o.concurrent_requests, o.requests_per_second), (0, 5, 7));
}

#[test]
fn bad_http_options_are_refused() {
    let prefix = "Failed parsing ClusterServerParameter 'httpOptions' :: caused by :: ";
    let cases = [
        (r#"{"bogus":1}"#, 40415, "BSON field 'ClusterServerParameter.bogus' is an unknown field."),
        (
            r#"{"cors":1}"#,
            14,
            "BSON field 'ClusterServerParameter.cors' is the wrong type 'int', expected type 'object'",
        ),
        (
            r#"{"cors":{"x":1}}"#,
            40415,
            "BSON field 'ClusterServerParameter.cors.x' is an unknown field.",
        ),
        (
            r#"{"cors":{"allowedOrigins":"a"}}"#,
            14,
            "BSON field 'ClusterServerParameter.cors.allowedOrigins' is the wrong type 'string', expected type 'array'",
        ),
        (
            r#"{"cors":{"allowedOrigins":[1]}}"#,
            14,
            "BSON field 'ClusterServerParameter.cors.allowedOrigins.0' is the wrong type 'int', expected type 'string'",
        ),
        (
            r#"{"cors":{"allowedOrigins":["*"]}}"#,
            2,
            "'*' is not an origin: give each origin as scheme://host[:port]",
        ),
        (
            r#"{"cors":{"allowedOrigins":["https://a.example/path"]}}"#,
            2,
            "'https://a.example/path' is not an origin: give each origin as scheme://host[:port]",
        ),
        (
            r#"{"rateLimits":{"concurrentRequests":0}}"#,
            2,
            "BSON field 'concurrentRequests' value must be >= 1, actual value '0'",
        ),
        (
            r#"{"rateLimits":{"requestsPerSecond":"x"}}"#,
            14,
            "BSON field 'ClusterServerParameter.rateLimits.requestsPerSecond' is the wrong type 'string', expected types '[long, int, double, decimal]'",
        ),
        (
            r#"{"rateLimits":{"y":1}}"#,
            40415,
            "BSON field 'ClusterServerParameter.rateLimits.y' is an unknown field.",
        ),
    ];
    for (value, code, msg) in cases {
        let mut m = Mem::new();
        let r = m.run("admin", set(HTTP_OPTIONS, value));
        assert_eq!(r.get("code"), Some(&Value::Int32(code)), "{value}: {r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(format!("{prefix}{msg}"))), "{value}");
    }
}

#[test]
fn mcp_options_are_set_and_checked() {
    let mut m = Mem::new();
    let r = m.run(
        "admin",
        set(MCP_OPTIONS, r#"{"readOnly":false,"allowDestructive":true,"maxDocuments":10,"maxBytes":2048,"maxTimeMS":500}"#),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(
        mcp_options(stored(&m, MCP_OPTIONS).as_deref()),
        McpOptions {
            read_only: false,
            allow_destructive: true,
            max_documents: 10,
            max_bytes: 2048,
            max_time_ms: 500
        }
    );
    let prefix = "Failed parsing ClusterServerParameter 'mcpOptions' :: caused by :: ";
    let cases = [
        (
            r#"{"readOnly":1}"#,
            14,
            "BSON field 'ClusterServerParameter.readOnly' is the wrong type 'int', expected type 'bool'",
        ),
        (
            r#"{"maxDocuments":0}"#,
            2,
            "BSON field 'maxDocuments' value must be >= 1, actual value '0'",
        ),
        (r#"{"z":true}"#, 40415, "BSON field 'ClusterServerParameter.z' is an unknown field."),
    ];
    for (value, code, msg) in cases {
        let r = m.run("admin", set(MCP_OPTIONS, value));
        assert_eq!(r.get("code"), Some(&Value::Int32(code)), "{value}: {r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(format!("{prefix}{msg}"))), "{value}");
    }
    // Fields not given keep their defaults.
    m.run("admin", set(MCP_OPTIONS, r#"{"maxDocuments":3}"#));
    let o = mcp_options(stored(&m, MCP_OPTIONS).as_deref());
    assert_eq!((o.read_only, o.max_documents, o.max_bytes), (true, 3, DEFAULT_MCP_MAX_BYTES));
}
