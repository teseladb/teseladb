//! OIDC authentication (013 FR-025): `oidcIdentityProviders`, the conversation of the drivers'
//! authentication specification (one step with `{jwt}`, or two with `{n}` first, answered with
//! the provider's `issuer`, `clientId` and `requestScopes`) and the validation of the access
//! token (RFC 7519, RS256, RS384 and RS512 against the provider's JWKS), without I/O. The user
//! is `<authNamePrefix>/<principal claim>` of `$external` and, with `useAuthorizationClaim`,
//! each group of the authorization claim is the role `<authNamePrefix>/<group>` of `admin`.
//! Checks and messages measured on the reference enterprise server 9.0.4.

use std::fmt;

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::rsa::RsaPublicKey;
use teseladb_x509::pem::base64_decode_strict;

use crate::users::{EXTERNAL_DB, RoleName, UserName};

/// The database of the roles of the authorization claim.
const ROLES_DB: &str = "admin";
/// `principalName` by default.
const DEFAULT_PRINCIPAL: &str = "sub";
/// Where a provider publishes its metadata, after its issuer (OpenID Connect Discovery 1.0).
pub const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";

/// Why a step failed; the client only learns `Authentication failed.`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcError(pub String);

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OidcError {}

fn fail<T>(m: impl Into<String>) -> Result<T, OidcError> {
    Err(OidcError(m.into()))
}

/// One provider of `oidcIdentityProviders`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// `issuer`.
    pub issuer: String,
    /// `audience`.
    pub audience: String,
    /// `authNamePrefix`.
    pub prefix: String,
    /// `clientId`.
    pub client_id: Option<String>,
    /// `requestScopes`.
    pub scopes: Vec<String>,
    /// `principalName`.
    pub principal: String,
    /// `useAuthorizationClaim`.
    pub use_authorization_claim: bool,
    /// `authorizationClaim`.
    pub authorization_claim: Option<String>,
    /// `supportsHumanFlows`.
    pub human_flows: bool,
}

fn text(d: &Document, key: &str) -> Result<Option<String>, OidcError> {
    match d.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => fail(format!("oidcIdentityProviders: '{key}' must be a string")),
    }
}

fn flag(d: &Document, key: &str, default: bool) -> Result<bool, OidcError> {
    match d.get(key) {
        None => Ok(default),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(_) => fail(format!("oidcIdentityProviders: '{key}' must be a boolean")),
    }
}

impl Provider {
    fn from_document(d: &Document) -> Result<Provider, OidcError> {
        let required = |k: &str| {
            text(d, k)?.ok_or_else(|| {
                OidcError(format!("oidcIdentityProviders: BSON field '{k}' is missing"))
            })
        };
        let scopes = match d.get("requestScopes") {
            None => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    _ => fail("oidcIdentityProviders: requestScopes must be strings"),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return fail("oidcIdentityProviders: requestScopes must be an array"),
        };
        let p = Provider {
            issuer: required("issuer")?,
            audience: required("audience")?,
            prefix: required("authNamePrefix")?,
            client_id: text(d, "clientId")?,
            scopes,
            principal: text(d, "principalName")?.unwrap_or_else(|| DEFAULT_PRINCIPAL.to_owned()),
            use_authorization_claim: flag(d, "useAuthorizationClaim", true)?,
            authorization_claim: text(d, "authorizationClaim")?,
            human_flows: flag(d, "supportsHumanFlows", true)?,
        };
        if p.use_authorization_claim && p.authorization_claim.is_none() {
            return fail(
                "oidcIdentityProviders: authorizationClaim is required when useAuthorizationClaim is true",
            );
        }
        if p.human_flows && p.client_id.is_none() {
            return fail(
                "oidcIdentityProviders: clientId is required when supportsHumanFlows is true",
            );
        }
        Ok(p)
    }
}

/// `oidcIdentityProviders`, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Providers(pub Vec<Provider>);

impl Providers {
    /// Reads the parameter: a JSON array of providers.
    pub fn parse(json: &str) -> Result<Providers, OidcError> {
        let wrapped = format!("{{\"p\": {json}}}");
        let doc = extjson::parse(&wrapped)
            .map_err(|e| OidcError(format!("oidcIdentityProviders: {e}")))?;
        let Some(Value::Array(items)) = doc.get("p") else {
            return fail("oidcIdentityProviders must be an array");
        };
        let mut out = Vec::new();
        for item in items {
            let Value::Document(d) = item else {
                return fail("oidcIdentityProviders: each provider is a document");
            };
            out.push(Provider::from_document(d)?);
        }
        Ok(Providers(out))
    }

    /// The provider of `issuer`.
    pub fn by_issuer(&self, issuer: &str) -> Option<&Provider> {
        self.0.iter().find(|p| p.issuer == issuer)
    }
}

/// The public keys of a provider's JWKS, by `kid`.
#[derive(Clone, Debug, Default)]
pub struct KeySet(pub Vec<(String, RsaPublicKey)>);

fn base64url(text: &str) -> Result<Vec<u8>, OidcError> {
    let mut s: String = text
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    base64_decode_strict(s.as_bytes()).map_err(|_| OidcError("invalid base64".into()))
}

fn json(bytes: &[u8]) -> Result<Document, OidcError> {
    let text = std::str::from_utf8(bytes).map_err(|_| OidcError("not UTF-8".into()))?;
    extjson::parse(text).map_err(|e| OidcError(e.to_string()))
}

impl KeySet {
    /// Reads a JWKS document; keys that are not RSA signing keys are left out.
    pub fn parse(body: &[u8]) -> Result<KeySet, OidcError> {
        let doc = json(body)?;
        let Some(Value::Array(keys)) = doc.get("keys") else { return fail("JWKS without keys") };
        let mut out = Vec::new();
        for k in keys {
            let Value::Document(k) = k else { continue };
            let field = |n: &str| match k.get(n) {
                Some(Value::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let (Some("RSA"), Some(kid), Some(n), Some(e)) =
                (field("kty"), field("kid"), field("n"), field("e"))
            else {
                continue;
            };
            if field("use").is_some_and(|u| u != "sig") {
                continue;
            }
            let key = RsaPublicKey::new(&base64url(n)?, &base64url(e)?)
                .map_err(|_| OidcError(format!("JWKS key '{kid}' is not an RSA key")))?;
            out.push((kid.to_owned(), key));
        }
        Ok(KeySet(out))
    }

    fn get(&self, kid: &str) -> Option<&RsaPublicKey> {
        self.0.iter().find(|(k, _)| k == kid).map(|(_, key)| key)
    }
}

/// The `jwks_uri` of a provider's discovery document.
pub fn jwks_uri(discovery: &[u8]) -> Result<String, OidcError> {
    match json(discovery)?.get("jwks_uri") {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => fail("the discovery document has no jwks_uri"),
    }
}

/// What a token proves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The user of `$external`.
    pub user: UserName,
    /// Its roles; `None` without `useAuthorizationClaim` (those of its document).
    pub roles: Option<Vec<RoleName>>,
}

/// A token whose header and claims are read, before its signature is checked.
#[derive(Clone, Debug)]
pub struct Token {
    /// `iss`.
    pub issuer: String,
    /// `kid`.
    pub kid: String,
    hash: HashAlgorithm,
    signed: Vec<u8>,
    signature: Vec<u8>,
    claims: Document,
}

fn number(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        // Seconds in JSON may be written with a fraction.
        Value::Double(f) if f.is_finite() => Some(f.floor() as i64),
        _ => None,
    }
}

impl Token {
    /// Splits and reads `jwt`.
    pub fn parse(jwt: &str) -> Result<Token, OidcError> {
        let mut parts = jwt.split('.');
        let (Some(h), Some(c), Some(s), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return fail("a JWT has three parts");
        };
        let header = json(&base64url(h)?)?;
        let claims = json(&base64url(c)?)?;
        let signature = base64url(s)?;
        let alg = match header.get("alg") {
            Some(Value::String(a)) => a.clone(),
            _ => return fail("BSON field 'JWSHeader.alg' is missing but a required field"),
        };
        let hash = match alg.as_str() {
            "RS256" => HashAlgorithm::Sha256,
            "RS384" => HashAlgorithm::Sha384,
            "RS512" => HashAlgorithm::Sha512,
            other => return fail(format!("Unknown hashing algorithm: '{other}'")),
        };
        let kid = match header.get("kid") {
            Some(Value::String(k)) => k.clone(),
            _ => return fail("BSON field 'JWSHeader.kid' is missing but a required field"),
        };
        let issuer = match claims.get("iss") {
            Some(Value::String(i)) => i.clone(),
            _ => return fail("BSON field 'JWT.iss' is missing but a required field"),
        };
        let signed = format!("{h}.{c}").into_bytes();
        Ok(Token { issuer, kid, hash, signed, signature, claims })
    }

    /// Checks the token with `keys` of its provider in `providers` at `now` (seconds), issued
    /// for the provider's `audience`.
    pub fn verify(
        &self,
        providers: &Providers,
        keys: &KeySet,
        now: i64,
    ) -> Result<Identity, OidcError> {
        self.verify_for(providers, keys, now, None)
    }

    /// [`Token::verify`], with the token issued for `resource` instead of the provider's
    /// `audience` when one is given: the URI of a protected resource, as MCP's tokens are
    /// (RFC 8707; 019 FR-027).
    pub fn verify_for(
        &self,
        providers: &Providers,
        keys: &KeySet,
        now: i64,
        resource: Option<&str>,
    ) -> Result<Identity, OidcError> {
        let p = providers
            .by_issuer(&self.issuer)
            .ok_or_else(|| OidcError(format!("Unknown Identity Provider '{}'", self.issuer)))?;
        let key = keys
            .get(&self.kid)
            .ok_or_else(|| OidcError(format!("Unknown JWT keyId << '{}'", self.kid)))?;
        if !key.verify_pkcs1v15(self.hash, &self.signed, &self.signature) {
            return fail("RSA Signature is invalid");
        }
        let c = &self.claims;
        let audience = match c.get("aud") {
            Some(Value::String(a)) => a.clone(),
            Some(Value::Array(a)) if a.len() == 1 => match &a[0] {
                Value::String(s) => s.clone(),
                _ => return fail("OIDC token must contain exactly one audience"),
            },
            _ => return fail("OIDC token must contain exactly one audience"),
        };
        if audience != resource.unwrap_or(&p.audience) {
            return fail(format!(
                "Unknown audience name '{audience}' for Identity Provider '{}'",
                p.issuer
            ));
        }
        let missing =
            |f: &str| OidcError(format!("BSON field 'JWT.{f}' is missing but a required field"));
        let exp = c.get("exp").and_then(number).ok_or_else(|| missing("exp"))?;
        if now >= exp {
            return fail("Token is expired");
        }
        if c.get("nbf").and_then(number).is_some_and(|nbf| now < nbf) {
            return fail("Token not yet valid");
        }
        if !matches!(c.get("sub"), Some(Value::String(_))) {
            return Err(missing("sub"));
        }
        let principal = match c.get(&p.principal) {
            Some(Value::String(s)) => s.clone(),
            _ => return fail(format!("Claim '{}' not found on OIDC token", p.principal)),
        };
        let user = UserName::new(format!("{}/{principal}", p.prefix), EXTERNAL_DB);
        let roles = match (&p.authorization_claim, p.use_authorization_claim) {
            (Some(claim), true) => Some(groups(c, claim, &p.prefix)?),
            _ => None,
        };
        Ok(Identity { user, roles })
    }
}

/// The roles of the authorization claim `claim`, sorted, as the reference server lists them.
fn groups(claims: &Document, claim: &str, prefix: &str) -> Result<Vec<RoleName>, OidcError> {
    let Some(v) = claims.get(claim) else {
        return fail(format!("Claim '{claim}' not found on OIDC token"));
    };
    let bad = || OidcError(format!("Authorization claim '{claim}' must be an array of strings"));
    let Value::Array(items) = v else { return Err(bad()) };
    let mut names = items
        .iter()
        .map(|g| match g {
            Value::String(s) => Ok(format!("{prefix}/{s}")),
            _ => Err(bad()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    names.dedup();
    Ok(names.into_iter().map(|n| RoleName::new(n, ROLES_DB)).collect())
}

/// What the client sent with `saslStart`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum First {
    /// One step: the access token.
    Jwt(String),
    /// Two steps: the provider's details, for the user named `n` if given.
    Ask(Option<String>),
}

/// Reads a client message: `{jwt}` (with `n`, which is ignored) or `{n}` alone, or nothing.
pub fn read_message(payload: &[u8]) -> Result<First, OidcError> {
    let doc =
        RawDoc::from_bytes(payload).map(Document::from).map_err(|e| OidcError(format!("{e:?}")))?;
    let (mut jwt, mut n) = (None, None);
    for (k, v) in doc.iter() {
        match (k, v) {
            ("jwt", Value::String(s)) => jwt = Some(s.clone()),
            ("n", Value::String(s)) => n = Some(s.clone()),
            ("jwt" | "n", _) => return fail(format!("BSON field '{k}' is the wrong type")),
            (other, _) => return fail(format!("BSON field '{other}' is an unknown field.")),
        }
    }
    Ok(match jwt {
        Some(t) => First::Jwt(t),
        None => First::Ask(n),
    })
}

/// The answer to `{n}`: the provider's `issuer`, its `clientId` when it supports human flows,
/// and `requestScopes`. With a name, the first provider; without one, the first that supports
/// human flows.
pub fn provider_reply(providers: &Providers, name: Option<&str>) -> Result<Vec<u8>, OidcError> {
    let p = match name {
        Some(_) => providers.0.first(),
        None => providers.0.iter().find(|p| p.human_flows),
    }
    .ok_or_else(|| {
        OidcError("Unable to determine identity provider, no provider supported human flows".into())
    })?;
    let mut d = Document::new().with("issuer", p.issuer.as_str());
    if let Some(c) = p.client_id.as_ref().filter(|_| p.human_flows) {
        d.push("clientId", c.as_str());
    }
    let scopes: Vec<Value> = p.scopes.iter().map(|s| Value::from(s.as_str())).collect();
    d.push("requestScopes", Value::Array(scopes));
    d.to_bytes().map_err(|e| OidcError(format!("{e:?}")))
}
