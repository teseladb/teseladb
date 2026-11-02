//! The metadata of the protected resource `/mcp` (RFC 9728, FR-027): public, at
//! `/.well-known/oauth-protected-resource/mcp` (and the same document without the suffix), with
//! the resource's URI, which tokens for MCP carry as audience (RFC 8707), and the authorization
//! servers of `oidcIdentityProviders`. A `401` of `/mcp` names it in `WWW-Authenticate`.

use teseladb_json::{Object, Value, to_string};

/// The path of the metadata of a resource at the root.
pub const WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";
/// The path of the metadata of `/mcp` (RFC 9728, 3.1: the resource's path after the prefix).
pub const WELL_KNOWN_MCP: &str = "/.well-known/oauth-protected-resource/mcp";

/// Whether `path` is one of the metadata paths.
#[must_use]
pub fn is_metadata_path(path: &str) -> bool {
    path == WELL_KNOWN || path == WELL_KNOWN_MCP
}

/// The URI of resource `/mcp` reached at `authority` (`host:port`) with `scheme`.
#[must_use]
pub fn resource_uri(scheme: &str, authority: &str) -> String {
    format!("{scheme}://{authority}/mcp")
}

/// The URL of the metadata of the resource `resource`, for `WWW-Authenticate` (RFC 9728, 3.1):
/// the well-known prefix between its authority and its path. `None` for a URI without
/// `scheme://`.
#[must_use]
pub fn metadata_url(resource: &str) -> Option<String> {
    let (scheme, rest) = resource.split_once("://")?;
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let path = path.split(['?', '#']).next().unwrap_or_default().trim_end_matches('/');
    Some(format!("{scheme}://{authority}{WELL_KNOWN}{path}"))
}

/// The metadata document of `resource` with the issuers of `issuers` and the scopes they ask
/// for, `scopes`.
#[must_use]
pub fn document(resource: &str, issuers: &[String], scopes: &[String]) -> String {
    let mut o = Object::new();
    o.insert("resource", resource);
    o.insert(
        "authorization_servers",
        Value::Array(issuers.iter().map(|i| Value::String(i.clone())).collect()),
    );
    o.insert("bearer_methods_supported", Value::Array(vec![Value::from("header")]));
    o.insert(
        "scopes_supported",
        Value::Array(scopes.iter().map(|s| Value::from(s.as_str())).collect()),
    );
    o.insert("resource_name", "TeselaDB MCP");
    to_string(&Value::Object(o)).unwrap_or_default()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_document_names_the_resource_and_its_issuers() {
        let uri = resource_uri("https", "db.example:27080");
        assert_eq!(uri, "https://db.example:27080/mcp");
        let d = document(&uri, &["https://idp.example".to_owned()], &["openid".to_owned()]);
        assert_eq!(
            d,
            r#"{"resource":"https://db.example:27080/mcp","authorization_servers":["https://idp.example"],"bearer_methods_supported":["header"],"scopes_supported":["openid"],"resource_name":"TeselaDB MCP"}"#
        );
        assert!(is_metadata_path(WELL_KNOWN) && is_metadata_path(WELL_KNOWN_MCP));
        assert!(!is_metadata_path("/mcp"));
        let url = |r: &str| metadata_url(r);
        assert_eq!(
            url("http://h:1/mcp").as_deref(),
            Some("http://h:1/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(
            url("https://db.example.com/api/mcp/?x=1").as_deref(),
            Some("https://db.example.com/.well-known/oauth-protected-resource/api/mcp")
        );
        assert_eq!(
            url("https://db.example.com").as_deref(),
            Some("https://db.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(url("db.example.com/mcp"), None);
    }
}
