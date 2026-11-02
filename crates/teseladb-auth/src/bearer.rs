//! The `Authorization: Bearer` credential of the HTTP API (019 FR-015, RFC 6750): an API key of
//! [`crate::apikey`] or an access token of the OIDC provider, told apart by the key's `tdb_`, and
//! the `WWW-Authenticate` challenge of a request without a valid one.

use crate::apikey;

/// The scheme, which RFC 9110 (11.1) compares without regard to case.
pub const SCHEME: &str = "Bearer";
/// The realm of the challenge.
pub const REALM: &str = "teseladb";

/// What a `Bearer` credential carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bearer<'a> {
    /// An API key.
    ApiKey(&'a str),
    /// A JWT access token of the OIDC provider.
    Jwt(&'a str),
}

/// Why an `Authorization` header gives no credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BearerError {
    /// Another scheme (`Basic`…).
    Scheme,
    /// The scheme without a token, or a token with characters RFC 6750 (2.1) does not allow.
    Malformed,
}

/// Whether `b` is a character of `b64token` (RFC 6750, 2.1).
fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

/// Reads the value of an `Authorization` header.
pub fn parse(value: &str) -> Result<Bearer<'_>, BearerError> {
    let value = value.trim();
    let (scheme, rest) = value.split_once(' ').unwrap_or((value, ""));
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return Err(BearerError::Scheme);
    }
    let token = rest.trim_start_matches(' ');
    if token.is_empty() || !token.bytes().all(token_char) {
        return Err(BearerError::Malformed);
    }
    if token.starts_with(apikey::KEY_PREFIX) {
        return Ok(Bearer::ApiKey(token));
    }
    Ok(Bearer::Jwt(token))
}

/// The `WWW-Authenticate` of a `401`: the realm, with `error="invalid_token"` when a credential
/// came and failed (RFC 6750, 3.1), and the URL of the protected resource's metadata for MCP
/// (RFC 9728, 5.1).
#[must_use]
pub fn challenge(invalid: bool, resource_metadata: Option<&str>) -> String {
    let mut s = format!("{SCHEME} realm=\"{REALM}\"");
    if invalid {
        s.push_str(", error=\"invalid_token\"");
    }
    if let Some(url) = resource_metadata {
        s.push_str(&format!(", resource_metadata=\"{url}\""));
    }
    s
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_is_a_key_or_a_token_and_other_schemes_are_not() {
        let key = apikey::encode_key(&[7; apikey::KEY_BYTES]);
        assert_eq!(parse(&format!("Bearer {key}")), Ok(Bearer::ApiKey(&key)));
        assert_eq!(parse(&format!("bearer   {key} ")), Ok(Bearer::ApiKey(&key)));
        assert_eq!(parse("BEARER a.b.c"), Ok(Bearer::Jwt("a.b.c")));
        assert_eq!(parse("Basic dXNlcjpw"), Err(BearerError::Scheme));
        assert_eq!(parse("Bearer"), Err(BearerError::Malformed));
        assert_eq!(parse("Bearer a b"), Err(BearerError::Malformed));
        assert_eq!(parse("Bearer a\"b"), Err(BearerError::Malformed));
    }

    #[test]
    fn the_challenge_names_the_realm_the_error_and_the_metadata() {
        assert_eq!(challenge(false, None), "Bearer realm=\"teseladb\"");
        assert_eq!(
            challenge(true, Some("https://h/.well-known/oauth-protected-resource/mcp")),
            "Bearer realm=\"teseladb\", error=\"invalid_token\", resource_metadata=\"https://h/.well-known/oauth-protected-resource/mcp\""
        );
    }
}
