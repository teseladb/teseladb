//! The server side of the SASL conversations with the messages the reference server 9.0.4 gives
//! (measured on a reference router with `--keyFile` and a reference replica set member with TLS,
//! 30-10-2026; 013 T019, FR-008).

use teseladb_auth::sasl::{
    CONVERSATION_ID, Conversation, SERVER_NONCE_BYTES, SaslError, SaslMechanism, Step, scram_user,
    server_nonce, start_mechanism, x509_found, x509_user,
};
use teseladb_auth::scram::{
    ClientFirst, ClientSession, Credentials, Mechanism, ScramError, ServerSession,
};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, User, UserName, advertised_mechanisms};

const ADMIN: &str = "admin";
const PASSWORD: &str = "secret1";
const CLIENT_NONCE: &str = "abcdefghijklmnop";

fn user(name: &str, mechs: &[Mechanism]) -> User {
    let mut u = User::new(UserName::new(name, ADMIN));
    for &m in mechs {
        let prepared = m.prepare_password(name, PASSWORD, true).unwrap();
        let creds = Credentials::derive(m, &prepared, b"0123456789abcdef", 4096);
        match m {
            Mechanism::ScramSha1 => u.scram_sha1 = Some(creds),
            Mechanism::ScramSha256 => u.scram_sha256 = Some(creds),
        }
    }
    u
}

fn both() -> User {
    user("both", &[Mechanism::ScramSha1, Mechanism::ScramSha256])
}

/// Runs a whole conversation from the client's side; the steps' results.
fn converse(mech: Mechanism, password: &str, skip_empty: bool) -> Vec<Result<Step, SaslError>> {
    let prepared = mech.prepare_password("both", password, false).unwrap();
    let (mut client, first) = ClientSession::start(mech, "both", &prepared, CLIENT_NONCE);
    let (parsed, name) = scram_user(ADMIN, &first).unwrap();
    assert_eq!(name, UserName::new("both", ADMIN));
    let nonce = server_nonce(&[7; SERVER_NONCE_BYTES]);
    let (mut conv, server_first) =
        Conversation::start_scram(mech, &parsed, Some(both()), &nonce, skip_empty).unwrap();
    let client_final = client.respond(&server_first).unwrap();
    let mut out = vec![conv.step(&client_final)];
    if let Ok(Step::Continue(server_final)) = &out[0] {
        client.verify(server_final).unwrap();
        out.push(conv.step(b""));
    }
    out
}

#[test]
fn scram_conversation_ends_with_or_without_the_empty_step() {
    let done = |payload: Vec<u8>| Step::Done { payload, user: UserName::new("both", ADMIN) };
    for mech in [Mechanism::ScramSha1, Mechanism::ScramSha256] {
        let skipped = converse(mech, PASSWORD, true);
        assert_eq!(skipped.len(), 1);
        assert!(
            matches!(&skipped[0], Ok(Step::Done { payload, .. }) if payload.starts_with(b"v="))
        );
        let full = converse(mech, PASSWORD, false);
        assert!(matches!(&full[0], Ok(Step::Continue(p)) if p.starts_with(b"v=")));
        assert_eq!(full[1], Ok(done(Vec::new())));
        assert_eq!(converse(mech, "wrong", true), vec![Err(SaslError::AuthenticationFailed)]);
    }
    assert_eq!(CONVERSATION_ID, 1);
    assert_eq!(server_nonce(&[0; SERVER_NONCE_BYTES]).len(), 32);
}

#[test]
fn scram_start_refuses_unknown_users_and_missing_credentials() {
    let (first, _) = scram_user(ADMIN, b"n,,n=both,r=abcdefgh").unwrap();
    let start = |u: Option<User>, m| Conversation::start_scram(m, &first, u, "srv", true).err();
    assert_eq!(start(None, Mechanism::ScramSha256), Some(SaslError::AuthenticationFailed));
    let sha1 = user("both", &[Mechanism::ScramSha1]);
    assert_eq!(
        start(Some(sha1), Mechanism::ScramSha256),
        Some(SaslError::NoCredentials(Mechanism::ScramSha256))
    );
    for bad in [
        &b"garbage"[..],
        b"p=tls-unique,,n=both,r=abcdefgh",
        b"n,,m=x,n=both,r=abcdefgh",
        b"n,,n=,r=abcdefgh",
        b"n,,n=a=2Xb,r=abcdefgh",
        b"n,a=other,n=both,r=abcdefgh",
        b"n,b=both,n=both,r=abcdefgh",
        b"n,a=both",
    ] {
        assert_eq!(scram_user(ADMIN, bad).err(), Some(SaslError::AuthenticationFailed), "{bad:?}");
    }
}

/// The reference server takes an authorization identity equal to the user, and in
/// `client-final-message` any of `n,,`, `y,,` and the header the client sent first (measured): a
/// proof signed over `c=biws` verifies after `n,a=both,`, and another accepted header reaches the
/// proof check (which fails, since the client signed `c=biws`) instead of the header check.
#[test]
fn authzid_and_channel_binding_headers_as_the_reference_takes_them() {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password("both", PASSWORD, false).unwrap();
    let creds = both().scram_sha256.unwrap();
    for (gs2, echo, expected) in [
        ("n,a=both,", None, Ok(())),
        ("n,a=both,", Some("n,a=both,"), Err(ScramError::AuthenticationFailed)),
        ("n,,", Some("y,,"), Err(ScramError::AuthenticationFailed)),
        ("n,,", Some("p=x,,"), Err(ScramError::Malformed("channel binding"))),
    ] {
        let (mut client, first) = ClientSession::start(mech, "both", &prepared, CLIENT_NONCE);
        let first = [gs2.as_bytes(), &first[3..]].concat();
        let parsed = ClientFirst::parse(&first).unwrap();
        let (mut server, server_first) = ServerSession::start(mech, &parsed, creds.clone(), "srv");
        let mut client_final = String::from_utf8(client.respond(&server_first).unwrap()).unwrap();
        if let Some(e) = echo {
            client_final = client_final.replacen("c=biws", &format!("c={}", b64(e)), 1);
        }
        assert_eq!(server.finish(client_final.as_bytes()).map(|_| ()), expected, "{gs2} {echo:?}");
    }
}

fn b64(s: &str) -> String {
    teseladb_x509::pem::base64_encode(s.as_bytes())
}

#[test]
fn mechanisms_known_enabled_and_on_external() {
    for m in SaslMechanism::ALL {
        assert_eq!(SaslMechanism::from_name(m.name()), Some(m));
    }
    assert_eq!(SaslMechanism::from_name("scram-sha-256"), None);
    assert_eq!(start_mechanism("SCRAM-SHA-256", ADMIN, &[]), Ok(SaslMechanism::ScramSha256));
    assert_eq!(
        start_mechanism(teseladb_names::SASL_X509, EXTERNAL_DB, &[]),
        Ok(SaslMechanism::X509)
    );
    let not_enabled =
        |m: &str| format!("Received authentication for mechanism {m} which is not enabled");
    let not_enabled_aws = not_enabled(teseladb_names::SASL_AWS);
    let not_enabled_oidc = not_enabled(teseladb_names::SASL_OIDC);
    let cases = [
        ("FOO", ADMIN, "Received authentication for mechanism FOO which is not known"),
        ("PLAIN", EXTERNAL_DB, "Received authentication for mechanism PLAIN which is not enabled"),
        ("GSSAPI", ADMIN, "Received authentication for mechanism GSSAPI which is not enabled"),
        (teseladb_names::SASL_AWS, ADMIN, not_enabled_aws.as_str()),
        (teseladb_names::SASL_OIDC, ADMIN, not_enabled_oidc.as_str()),
        (
            "SCRAM-SHA-256",
            EXTERNAL_DB,
            "Unsupported mechanism 'SCRAM-SHA-256' on authentication database '$external'",
        ),
    ];
    for (name, db, msg) in cases {
        assert_eq!(start_mechanism(name, db, &[]).unwrap_err().to_string(), msg);
    }
    // `authenticationMechanisms` replaces the default list.
    let plain = [SaslMechanism::Plain, SaslMechanism::ScramSha256];
    assert_eq!(start_mechanism("PLAIN", EXTERNAL_DB, &plain), Ok(SaslMechanism::Plain));
    assert!(start_mechanism("SCRAM-SHA-1", ADMIN, &plain).is_err());
    assert_eq!(SaslMechanism::X509.scram(), None);
    assert_eq!(SaslMechanism::ScramSha1.scram(), Some(Mechanism::ScramSha1));
}

#[test]
fn error_messages_are_the_reference_servers() {
    let cases = [
        (
            SaslError::NoCredentials(Mechanism::ScramSha256),
            "Unable to use SCRAM-SHA-256 based authentication for user without any SCRAM-SHA-256 credentials registered",
        ),
        (SaslError::AuthenticationFailed, "Authentication failed."),
        (SaslError::NoConversation, "No SASL session state found"),
        (
            SaslError::UserNotFound(UserName::new("CN=client,O=TeselaDB", EXTERNAL_DB)),
            "Could not find user \"CN=client,O=TeselaDB\" for db \"$external\"",
        ),
    ];
    for (e, msg) in cases {
        assert_eq!(e.to_string(), msg);
    }
}

#[test]
fn x509_checks_database_certificate_and_user() {
    let peer = "CN=client,O=TeselaDB";
    let name = UserName::new(peer, EXTERNAL_DB);
    assert_eq!(x509_user(EXTERNAL_DB, None, Some(peer)), Ok(name.clone()));
    assert_eq!(x509_user(EXTERNAL_DB, Some(peer), Some(peer)), Ok(name.clone()));
    for (db, requested, cert) in [
        (ADMIN, None, Some(peer)),
        (EXTERNAL_DB, None, None),
        (EXTERNAL_DB, Some("CN=other"), Some(peer)),
    ] {
        assert_eq!(x509_user(db, requested, cert), Err(SaslError::AuthenticationFailed));
    }
    assert_eq!(x509_found(name.clone(), true, false), Ok(name.clone()));
    assert_eq!(x509_found(name.clone(), false, true), Err(SaslError::AuthenticationFailed));
    assert_eq!(x509_found(name.clone(), false, false), Err(SaslError::UserNotFound(name)));
}

#[test]
fn advertised_mechanisms_hide_which_users_exist() {
    let sha1 = user("s1", &[Mechanism::ScramSha1]);
    let sha256 = user("s256", &[Mechanism::ScramSha256]);
    assert_eq!(advertised_mechanisms(&sha1.name, Some(&sha1), &[]), ["SCRAM-SHA-1"]);
    assert_eq!(advertised_mechanisms(&sha256.name, Some(&sha256), &[]), ["SCRAM-SHA-256"]);
    let both = both();
    let all = ["SCRAM-SHA-1", "SCRAM-SHA-256"];
    assert_eq!(advertised_mechanisms(&both.name, Some(&both), &[]), all);
    let nobody = UserName::new("nobody", ADMIN);
    assert_eq!(advertised_mechanisms(&nobody, None, &[]), all);
    let ext = UserName::new("CN=x", EXTERNAL_DB);
    assert_eq!(advertised_mechanisms(&ext, None, &[]), [teseladb_names::SASL_X509]);
    // Only the enabled mechanisms are listed, as the reference server 9.0.4 does with
    // `authenticationMechanisms=SCRAM-SHA-256` (measured): also in FIPS mode.
    let sha256_only = [SaslMechanism::ScramSha256, SaslMechanism::X509];
    assert_eq!(advertised_mechanisms(&both.name, Some(&both), &sha256_only), ["SCRAM-SHA-256"]);
    assert_eq!(advertised_mechanisms(&nobody, None, &sha256_only), ["SCRAM-SHA-256"]);
    assert!(advertised_mechanisms(&sha1.name, Some(&sha1), &sha256_only).is_empty());
    assert!(advertised_mechanisms(&ext, None, &[SaslMechanism::ScramSha256]).is_empty());
    assert!(SaslMechanism::ScramSha1.is_enabled(&[]));
    assert!(!SaslMechanism::Plain.is_enabled(&[]));
    assert!(SaslMechanism::Plain.is_enabled(&[SaslMechanism::Plain]));
}

#[test]
fn names_as_the_reference_writes_them() {
    let u = UserName::from_dotted("$external.CN=a.b").unwrap();
    assert_eq!(u, UserName::new("CN=a.b", EXTERNAL_DB));
    assert_eq!(u.to_string(), "$external.CN=a.b");
    assert_eq!(UserName::from_dotted("nodot"), None);
    let r = RoleName::new("read", "test");
    assert_eq!((r.role.as_str(), r.db.as_str()), ("read", "test"));
    assert_eq!(both().credentials(Mechanism::ScramSha1).map(|c| c.iterations), Some(4096));
    assert!(User::new(u).roles.is_empty());
}
