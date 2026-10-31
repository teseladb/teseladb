//! REFDB-OIDC (013 T036, FR-025): `oidcIdentityProviders`, the JWKS, the validation of the
//! access token and the two-step answer, with the checks and messages RefDb Enterprise 9.0.4
//! gives (measured with a test provider; `tests/fixtures/oidc/rsa2048.pem` is a key made for
//! these tests).

use teseladb_auth::oidc::{self, First, KeySet, OidcError, Providers, Token};
use teseladb_auth::users::{RoleName, UserName};
use teseladb_bson::{Document, Value};
use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::rsa::RsaPrivateKey;
use teseladb_x509::PrivateKey;

const ISSUER: &str = "https://idp.test:8443";
const NOW: i64 = 1_800_000_000;

fn key() -> RsaPrivateKey {
    let pem = include_str!("fixtures/oidc/rsa2048.pem");
    match PrivateKey::from_pem(pem, None).unwrap() {
        PrivateKey::Rsa(k, _) => k,
        _ => panic!("not RSA"),
    }
}

fn b64url(bytes: &[u8]) -> String {
    teseladb_x509::pem::base64_encode(bytes)
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect()
}

fn jwks(kid: &str) -> Vec<u8> {
    let k = key();
    let p = k.public_key();
    format!(
        r#"{{"keys": [{{"kty": "EC", "kid": "ec"}}, {{"kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB"}}, {{"kty": "RSA", "kid": "{kid}", "use": "sig", "alg": "RS256", "n": "{}", "e": "{}"}}]}}"#,
        b64url(&p.modulus()),
        b64url(&p.exponent())
    )
    .into_bytes()
}

fn token(header: &str, claims: &str, hash: HashAlgorithm) -> String {
    let signed = format!("{}.{}", b64url(header.as_bytes()), b64url(claims.as_bytes()));
    let sig = key().sign_pkcs1v15(hash, signed.as_bytes()).unwrap();
    format!("{signed}.{}", b64url(&sig))
}

fn rs256(claims: &str) -> String {
    token(r#"{"alg":"RS256","typ":"JWT","kid":"k1"}"#, claims, HashAlgorithm::Sha256)
}

fn providers() -> Providers {
    Providers::parse(&format!(
        r#"[{{"issuer": "{ISSUER}", "audience": "teseladb", "authNamePrefix": "idp", "clientId": "cid", "requestScopes": ["s1"], "useAuthorizationClaim": true, "authorizationClaim": "groups", "supportsHumanFlows": false}},
           {{"issuer": "https://other.test", "audience": "a", "authNamePrefix": "o", "useAuthorizationClaim": false, "principalName": "email", "clientId": "c2"}}]"#
    ))
    .unwrap()
}

fn verify(jwt: &str) -> Result<oidc::Identity, OidcError> {
    let keys = KeySet::parse(&jwks("k1")).unwrap();
    Token::parse(jwt)?.verify(&providers(), &keys, NOW)
}

fn claims(extra: &str) -> String {
    format!(r#"{{"iss":"{ISSUER}","aud":"teseladb","sub":"alice","exp":{},{extra}}}"#, NOW + 600)
}

#[test]
fn a_valid_token_names_the_user_and_its_groups() {
    let id = verify(&rs256(&claims(r#""groups":["readers","others","readers"]"#))).unwrap();
    assert_eq!(id.user, UserName::new("idp/alice", "$external"));
    assert_eq!(
        id.roles,
        Some(vec![RoleName::new("idp/others", "admin"), RoleName::new("idp/readers", "admin")])
    );
    // RS384 and RS512, an audience in an array of one, and a fractional expiry.
    for (alg, hash) in [("RS384", HashAlgorithm::Sha384), ("RS512", HashAlgorithm::Sha512)] {
        let h = format!(r#"{{"alg":"{alg}","kid":"k1"}}"#);
        assert!(verify(&token(&h, &claims(r#""groups":[]"#), hash)).is_ok(), "{alg}");
    }
    let c = format!(
        r#"{{"iss":"{ISSUER}","aud":["teseladb"],"sub":"a","exp":{}.5,"groups":[]}}"#,
        NOW + 1
    );
    assert!(verify(&rs256(&c)).is_ok());
    // A provider without the authorization claim, with another principal claim.
    let keys = KeySet::parse(&jwks("k1")).unwrap();
    let c = format!(
        r#"{{"iss":"https://other.test","aud":"a","sub":"s","email":"e@x","exp":{}}}"#,
        NOW + 60
    );
    let id = Token::parse(&rs256(&c)).unwrap().verify(&providers(), &keys, NOW).unwrap();
    assert_eq!((id.user.user.as_str(), id.roles), ("o/e@x", None));
}

#[test]
fn bad_tokens_fail_with_refdbs_messages() {
    let err = |jwt: &str| verify(jwt).unwrap_err().0;
    let g = r#""groups":["r"]"#;
    let at = |exp: i64, extra: &str| {
        format!(
            r#"{{"iss":"{ISSUER}","aud":"teseladb","sub":"alice","exp":{exp}{extra},"groups":["r"]}}"#
        )
    };
    assert_eq!(err(&rs256(&at(NOW - 60, ""))), "Token is expired");
    // A time that is not a finite number is no time at all.
    let infinite = verify(&rs256(&claims(g).replace(&format!("{}", NOW + 600), "1e999")));
    let e = infinite.unwrap_err();
    assert_eq!(e.to_string(), e.0);
    assert_eq!(
        err(&rs256(&at(NOW + 600, &format!(r#","nbf":{}"#, NOW + 300)))),
        "Token not yet valid"
    );
    let other_aud = claims(g).replace(r#""aud":"teseladb""#, r#""aud":"other""#);
    assert_eq!(
        err(&rs256(&other_aud)),
        format!("Unknown audience name 'other' for Identity Provider '{ISSUER}'")
    );
    let two = claims(g).replace(r#""aud":"teseladb""#, r#""aud":["x","teseladb"]"#);
    assert_eq!(err(&rs256(&two)), "OIDC token must contain exactly one audience");
    let num = claims(g).replace(r#""aud":"teseladb""#, r#""aud":[1]"#);
    assert_eq!(err(&rs256(&num)), "OIDC token must contain exactly one audience");
    let evil = claims(g).replace(ISSUER, "https://evil:8443");
    assert_eq!(err(&rs256(&evil)), "Unknown Identity Provider 'https://evil:8443'");
    assert_eq!(err(&rs256(&claims(r#""x":1"#))), "Claim 'groups' not found on OIDC token");
    assert_eq!(
        err(&rs256(&claims(r#""groups":"single""#))),
        "Authorization claim 'groups' must be an array of strings"
    );
    assert_eq!(
        err(&rs256(&claims(r#""groups":[1]"#))),
        "Authorization claim 'groups' must be an array of strings"
    );
    let no_sub = claims(g).replace(r#""sub":"alice","#, "");
    assert_eq!(err(&rs256(&no_sub)), "BSON field 'JWT.sub' is missing but a required field");
    let no_exp = format!(r#"{{"iss":"{ISSUER}","aud":"teseladb","sub":"a",{g}}}"#);
    assert_eq!(err(&rs256(&no_exp)), "BSON field 'JWT.exp' is missing but a required field");
    let no_iss = r#"{"aud":"teseladb","sub":"a","exp":1}"#;
    assert_eq!(err(&rs256(no_iss)), "BSON field 'JWT.iss' is missing but a required field");
    let kid = token(r#"{"alg":"RS256","kid":"zz"}"#, &claims(g), HashAlgorithm::Sha256);
    assert_eq!(err(&kid), "Unknown JWT keyId << 'zz'");
    let no_kid = token(r#"{"alg":"RS256"}"#, &claims(g), HashAlgorithm::Sha256);
    assert_eq!(err(&no_kid), "BSON field 'JWSHeader.kid' is missing but a required field");
    let hs = token(r#"{"alg":"HS256","kid":"k1"}"#, &claims(g), HashAlgorithm::Sha256);
    assert_eq!(err(&hs), "Unknown hashing algorithm: 'HS256'");
    let no_alg = token(r#"{"kid":"k1"}"#, &claims(g), HashAlgorithm::Sha256);
    assert!(err(&no_alg).contains("JWSHeader.alg"));
    // A signature of another hash, a changed claim, garbage.
    let wrong = token(r#"{"alg":"RS256","kid":"k1"}"#, &claims(g), HashAlgorithm::Sha384);
    assert_eq!(err(&wrong), "RSA Signature is invalid");
    let good = rs256(&claims(g));
    let parts: Vec<&str> = good.split('.').collect();
    let forged =
        format!("{}.{}.{}", parts[0], b64url(claims(r#""groups":["admin"]"#).as_bytes()), parts[2]);
    assert_eq!(err(&forged), "RSA Signature is invalid");
    assert_eq!(err("a.b!.c"), "invalid base64");
    assert_eq!(err("a.b"), "a JWT has three parts");
    assert_eq!(err("a.b.c.d"), "a JWT has three parts");
    assert!(verify(&format!("{}.{}.x", b64url(b"\xff"), parts[1])).is_err());
    assert!(verify(&format!("{}.{}.x", b64url(b"[1]x"), parts[1])).is_err());
}

#[test]
fn providers_jwks_and_discovery() {
    let p = providers();
    assert_eq!(p.0[0].scopes, vec!["s1".to_owned()]);
    assert_eq!((p.0[1].principal.as_str(), p.0[1].human_flows), ("email", true));
    for bad in [
        r#"{"issuer": "x"}"#,
        r#"[1]"#,
        r#"[{"audience": "a", "authNamePrefix": "p", "supportsHumanFlows": false, "useAuthorizationClaim": false}]"#,
        r#"[{"issuer": "i", "audience": "a", "authNamePrefix": "p", "supportsHumanFlows": false}]"#,
        r#"[{"issuer": "i", "audience": "a", "authNamePrefix": "p", "useAuthorizationClaim": false}]"#,
        r#"[{"issuer": 1, "audience": "a", "authNamePrefix": "p"}]"#,
        r#"[{"issuer": "i", "audience": "a", "authNamePrefix": "p", "supportsHumanFlows": "no"}]"#,
        r#"[{"issuer": "i", "audience": "a", "authNamePrefix": "p", "requestScopes": "s"}]"#,
        r#"[{"issuer": "i", "audience": "a", "authNamePrefix": "p", "requestScopes": [1]}]"#,
        "[",
    ] {
        assert!(Providers::parse(bad).is_err(), "{bad}");
    }
    assert!(KeySet::parse(br#"{"x": 1}"#).is_err());
    assert!(
        KeySet::parse(br#"{"keys": [1, {"kty": "RSA", "kid": "k", "n": "AA", "e": "AQAB"}]}"#)
            .is_err()
    );
    assert!(KeySet::parse(b"\xff").is_err());
    assert_eq!(
        oidc::jwks_uri(br#"{"jwks_uri": "https://i/jwks"}"#),
        Ok("https://i/jwks".to_owned())
    );
    assert!(oidc::jwks_uri(br#"{"issuer": "i"}"#).is_err());
}

#[test]
fn the_first_message_and_the_providers_answer() {
    let doc = |d: Document| d.to_bytes().unwrap();
    assert_eq!(
        oidc::read_message(&doc(Document::new().with("jwt", "t").with("n", "x"))),
        Ok(First::Jwt("t".into()))
    );
    assert_eq!(
        oidc::read_message(&doc(Document::new().with("n", "alice"))),
        Ok(First::Ask(Some("alice".into())))
    );
    assert_eq!(oidc::read_message(&doc(Document::new())), Ok(First::Ask(None)));
    assert!(oidc::read_message(&doc(Document::new().with("jwt", 1))).is_err());
    assert!(oidc::read_message(&doc(Document::new().with("z", "1"))).is_err());
    assert!(oidc::read_message(b"xyz").is_err());
    // With a name: the first provider, without its client when it has no human flows.
    let reply = oidc::provider_reply(&providers(), Some("alice")).unwrap();
    let d = Document::from(teseladb_bson::RawDoc::from_bytes(&reply).unwrap());
    assert_eq!(d.get("issuer"), Some(&Value::from(ISSUER)));
    assert_eq!(d.get("clientId"), None);
    assert_eq!(d.get("requestScopes"), Some(&Value::Array(vec![Value::from("s1")])));
    // Without one: the first with human flows, with its client.
    let reply = oidc::provider_reply(&providers(), None).unwrap();
    let d = Document::from(teseladb_bson::RawDoc::from_bytes(&reply).unwrap());
    assert_eq!(d.get("clientId"), Some(&Value::from("c2")));
    let only_machine = Providers(vec![providers().0[0].clone()]);
    assert_eq!(
        oidc::provider_reply(&only_machine, None).unwrap_err().0,
        "Unable to determine identity provider, no provider supported human flows"
    );
}
