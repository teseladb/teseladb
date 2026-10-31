//! PLAIN (013 T036, FR-025) as RefDb Enterprise 9.0.4 takes it (measured 30-10-2026 with an
//! OpenLDAP server): the message's forms and its refusals with RefDb's log messages, and the
//! password of a local user checked against its SCRAM credentials.

use teseladb_auth::plain::{self, PlainError};
use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_auth::users::{User, UserName};

#[test]
fn messages_and_their_refusals() {
    let p = plain::parse(b"\0jane\0janepw").unwrap();
    assert_eq!((p.user.as_str(), p.password.as_slice()), ("jane", &b"janepw"[..]));
    assert_eq!(format!("{p:?}"), "Plain { user: \"jane\" }");
    assert_eq!(plain::parse(b"jane\0jane\0pw").unwrap().user, "jane");
    let cases: [(&[u8], PlainError); 6] = [
        (b"janepw", PlainError::MissingFirstNul),
        (b"\0janepw", PlainError::MissingSecondNul),
        (b"other\0jane\0pw", PlainError::OtherIdentity),
        (b"\0\0pw", PlainError::EmptyUser),
        (b"\0jane\0", PlainError::EmptyPassword),
        (b"\0\xff\0pw", PlainError::NotUtf8),
    ];
    for (payload, want) in cases {
        assert_eq!(plain::parse(payload), Err(want));
    }
    assert_eq!(
        PlainError::MissingFirstNul.to_string(),
        "Incorrectly formatted PLAIN client message, missing first NULL delimiter"
    );
    assert_eq!(
        PlainError::OtherIdentity.to_string(),
        "SASL authorization identity must match authentication identity"
    );
    for e in [
        PlainError::MissingSecondNul,
        PlainError::EmptyUser,
        PlainError::EmptyPassword,
        PlainError::NotUtf8,
    ] {
        assert!(e.to_string().starts_with("Incorrectly formatted PLAIN client message"));
    }
}

fn user(mechs: &[Mechanism]) -> User {
    let mut u = User::new(UserName::new("root", "admin"));
    for &m in mechs {
        let prepared = m.prepare_password("root", "rootpw", m == Mechanism::ScramSha1).unwrap();
        let creds = Credentials::derive(m, &prepared, b"0123456789abcdef", 4096);
        match m {
            Mechanism::ScramSha1 => u.scram_sha1 = Some(creds),
            Mechanism::ScramSha256 => u.scram_sha256 = Some(creds),
        }
    }
    u
}

#[test]
fn local_users_are_checked_against_their_scram_credentials() {
    for mechs in [
        &[Mechanism::ScramSha256][..],
        &[Mechanism::ScramSha1],
        &[Mechanism::ScramSha1, Mechanism::ScramSha256],
    ] {
        let u = user(mechs);
        assert!(plain::check_local(&u, b"rootpw"), "{mechs:?}");
        assert!(!plain::check_local(&u, b"wrong"), "{mechs:?}");
        assert!(!plain::check_local(&u, b"\xff"), "{mechs:?}");
    }
    assert!(!plain::check_local(&user(&[]), b"rootpw"));
    // A password SASLprep refuses (a control character) never matches.
    assert!(!plain::check_local(&user(&[Mechanism::ScramSha256]), b"a\x07b"));
}
