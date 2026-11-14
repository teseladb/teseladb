//! Fuzzing of the parsers of the federated mechanisms (013 T010, FR-025, SC-003): the GSS-API
//! `AP-REQ` and MIT keytabs of Kerberos, the JWT, JWKS, discovery document and messages of OIDC
//! authentication, and the messages and STS answers of AWS IAM authentication. Real inputs, made as
//! the tests of each mechanism make them, are mutated (bit flips, cuts, insertions, spliced bytes)
//! and fed to the parsers, which must answer or fail and never panic. A mutated Kerberos token or
//! JWT that is still accepted names the same user as the original: a mutation never forges an
//! identity.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

#[path = "support/krb_initiator.rs"]
mod krb;

use std::time::{Duration, Instant};

use krb::{NOW, ap_req, initiator_wrap, keytab, keytab_entry, options};
use teseladb_auth::aws::{self, Server, SigV4};
use teseladb_auth::kerberos;
use teseladb_auth::oidc::{self, KeySet, Providers, Token};
use teseladb_bson::{Document, Value};
use teseladb_crypto::HashAlgorithm;
use teseladb_types::Rng;
use teseladb_x509::PrivateKey;

const DEFAULT_ROUNDS: u64 = 6_000;
const ISSUER: &str = "https://idp.test:8443";
const STS_HOST: &str = "sts.amazonaws.com";
const KERBEROS_USER: &str = "alice@TESELA.TEST";

/// Bytes a mutation inserts: the delimiters and markers of DER, JSON, base64url, XML and BSON.
const INSERTED: [u8; 12] = [0x30, 0x60, 0xa0, 0x04, b'.', b'"', b'{', b'-', b'_', b'<', 0, 0xff];

fn mutate(rng: &mut Rng, seeds: &[&[u8]]) -> Vec<u8> {
    let mut out = seeds[rng.range(0, seeds.len() as u64) as usize].to_vec();
    for _ in 0..=rng.range(0, 4) {
        let len = out.len() as u64;
        match rng.range(0, 5) {
            0 if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] ^= 1 << rng.range(0, 8);
            }
            1 if len > 0 => out.truncate(rng.range(0, len) as usize),
            2 => {
                let i = rng.range(0, len + 1) as usize;
                out.insert(i, INSERTED[rng.range(0, INSERTED.len() as u64) as usize]);
            }
            3 => {
                let other = seeds[rng.range(0, seeds.len() as u64) as usize];
                let cut = rng.range(0, other.len() as u64 + 1) as usize;
                let at = rng.range(0, len + 1) as usize;
                out.splice(at..at, other[..cut].iter().copied());
            }
            _ if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] = rng.range(0, 256) as u8;
            }
            _ => {}
        }
    }
    out
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

fn bson(d: &Document) -> Vec<u8> {
    d.to_bytes().unwrap()
}

/// The real inputs of every mechanism, and what reading them needs.
struct Seeds {
    keys: Vec<kerberos::ServiceKey>,
    krb: Vec<Vec<u8>>,
    providers: Providers,
    key_set: KeySet,
    jwt: Vec<u8>,
    oidc: Vec<Vec<u8>>,
    aws: Vec<Vec<u8>>,
}

fn seeds() -> Seeds {
    // Kerberos: the initiator's token, and a keytab with an entry of each kind.
    let mut tab = vec![0x05, 0x02];
    tab.extend_from_slice(&keytab_entry(
        &[teseladb_names::KERBEROS_SERVICE, "db.test"],
        "TESELA.TEST",
        18,
        &[7; 32],
        3,
    ));
    tab.extend_from_slice(&keytab_entry(
        &[teseladb_names::KERBEROS_SERVICE, "db.test"],
        "TESELA.TEST",
        23,
        &[1; 16],
        3,
    ));
    tab.extend_from_slice(&(-4i32).to_be_bytes());
    tab.extend_from_slice(&[0; 4]);
    // OIDC: a token signed with the test key, its JWKS, the providers and a discovery document.
    let PrivateKey::Rsa(key, _) =
        PrivateKey::from_pem(include_str!("fixtures/oidc/rsa2048.pem"), None).unwrap()
    else {
        panic!("the OIDC test key is not RSA")
    };
    let public = key.public_key();
    let jwks = format!(
        r#"{{"keys": [{{"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256", "n": "{}", "e": "{}"}}]}}"#,
        b64url(&public.modulus()),
        b64url(&public.exponent())
    );
    let claims = format!(
        r#"{{"iss": "{ISSUER}", "sub": "alice", "aud": "teseladb", "exp": {}, "iat": {}, "groups": ["readers"]}}"#,
        NOW + 600,
        NOW - 10
    );
    let signed = format!(
        "{}.{}",
        b64url(br#"{"alg":"RS256","typ":"JWT","kid":"k1"}"#),
        b64url(claims.as_bytes())
    );
    let signature = key.sign_pkcs1v15(HashAlgorithm::Sha256, signed.as_bytes()).unwrap();
    let jwt = format!("{signed}.{}", b64url(&signature));
    let providers = Providers::parse(&format!(
        r#"[{{"issuer": "{ISSUER}", "audience": "teseladb", "authNamePrefix": "idp", "clientId": "cid", "useAuthorizationClaim": true, "authorizationClaim": "groups"}}]"#
    ))
    .unwrap();
    let discovery = format!(r#"{{"issuer": "{ISSUER}", "jwks_uri": "{ISSUER}/keys"}}"#);
    // AWS: the client's two messages and the STS answer.
    let first = bson(
        &Document::new().with("r", Value::Binary { subtype: 0, bytes: vec![1; 32] }).with("p", 110),
    );
    let date = "20261030T215406Z";
    let nonce = teseladb_x509::pem::base64_encode(&[[1u8; 32], [2u8; 32]].concat());
    let headers = [
        ("Content-Length", "43"),
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Host", STS_HOST),
        ("X-Amz-Date", date),
        (teseladb_names::AWS_HEADER_CHANNEL_BINDING, "n"),
        (teseladb_names::AWS_HEADER_SERVER_NONCE, nonce.as_str()),
    ];
    let authorization = aws::authorization(&SigV4 {
        method: "POST",
        path: "/",
        query: "",
        headers: &headers,
        body: aws::STS_BODY.as_bytes(),
        region: "us-east-1",
        service: "sts",
        date,
        key_id: "AKIDUSER",
        secret: b"secret",
    });
    let second = bson(&Document::new().with("a", authorization).with("d", date));
    let sts = "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Arn>arn:aws:sts::123456789012:assumed-role/reader/session1</Arn><UserId>AIDA</UserId><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>";
    Seeds {
        keys: keytab(),
        krb: vec![ap_req(&options()), tab],
        providers,
        key_set: KeySet::parse(jwks.as_bytes()).unwrap(),
        oidc: vec![
            jwt.clone().into_bytes(),
            jwks.into_bytes(),
            discovery.into_bytes(),
            bson(&Document::new().with("jwt", jwt.as_str())),
            bson(&Document::new().with("n", "alice")),
        ],
        jwt: jwt.into_bytes(),
        aws: vec![
            first,
            second,
            sts.as_bytes().to_vec(),
            format!("https://{STS_HOST}").into_bytes(),
        ],
    }
}

fn kerberos_inputs(rng: &mut Rng, s: &Seeds) {
    let seeds: Vec<&[u8]> = s.krb.iter().map(Vec::as_slice).collect();
    let input = mutate(rng, &seeds);
    let _ = kerberos::parse_keytab(&input);
    if let Ok(accepted) = kerberos::accept(&input, &s.keys, NOW, [0; 16]) {
        assert_eq!(accepted.user.user, KERBEROS_USER, "a mutated AP-REQ named another user");
        // The initiator's wrap tokens, signed and sealed, mutated: unwrapping answers or fails.
        let wraps = [initiator_wrap(b"\x01\0\0\0alice", false), initiator_wrap(b"x", true)];
        let wrap_seeds: Vec<&[u8]> = wraps.iter().map(Vec::as_slice).collect();
        let _ = accepted.context.unwrap(&mutate(rng, &wrap_seeds));
    }
}

fn oidc_inputs(rng: &mut Rng, s: &Seeds) {
    let seeds: Vec<&[u8]> = s.oidc.iter().map(Vec::as_slice).collect();
    let input = mutate(rng, &seeds);
    let text = String::from_utf8_lossy(&input);
    let _ = Providers::parse(&text);
    let _ = KeySet::parse(&input);
    let _ = oidc::jwks_uri(&input);
    let _ = oidc::read_message(&input);
    if let Ok(token) = Token::parse(&text)
        && let Ok(identity) = token.verify(&s.providers, &s.key_set, NOW)
    {
        // Only the signed header and claims decide who logs in: a token that still verifies
        // after a mutation names the user of the original.
        let original = Token::parse(&String::from_utf8_lossy(&s.jwt))
            .and_then(|t| t.verify(&s.providers, &s.key_set, NOW))
            .unwrap();
        assert_eq!(identity.user, original.user, "a mutated JWT named another user");
    }
}

fn aws_inputs(rng: &mut Rng, s: &Seeds) {
    let seeds: Vec<&[u8]> = s.aws.iter().map(Vec::as_slice).collect();
    let input = mutate(rng, &seeds);
    let _ = aws::sts_host(&String::from_utf8_lossy(&input));
    let _ = aws::caller(200, &input);
    if let Ok((server, _)) = Server::start(&input, [2; 32], STS_HOST) {
        let _ = server.request(&mutate(rng, &seeds));
    }
    let (server, _) = Server::start(&s.aws[0], [2; 32], STS_HOST).unwrap();
    let _ = server.request(&input);
}

#[test]
fn federated_mechanisms_survive_mutated_inputs() {
    let s = seeds();
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|v| v.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|v| v.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_013b);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..30 {
            kerberos_inputs(&mut rng, &s);
            oidc_inputs(&mut rng, &s);
            aws_inputs(&mut rng, &s);
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(limit) if start.elapsed() < Duration::from_secs(limit) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < DEFAULT_ROUNDS => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} rounds in {:?}", start.elapsed());
}
