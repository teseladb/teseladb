//! SCRAM against the exchanges of RFC 5802 (section 5, SCRAM-SHA-1) and RFC 7677 (section 3,
//! SCRAM-SHA-256), from both sides, and the failures each side reports (013 T019, T044, FR-008).

use teseladb_auth::scram::{
    ClientFirst, ClientSession, Credentials, Mechanism, ScramError, ServerSession,
};
use teseladb_x509::pem::base64_decode;

struct Vector {
    mech: Mechanism,
    client_nonce: &'static str,
    server_nonce: &'static str,
    salt: &'static str,
    client_first: &'static str,
    server_first: &'static str,
    client_final: &'static str,
    server_final: &'static str,
}

const VECTORS: [Vector; 2] = [
    Vector {
        mech: Mechanism::ScramSha1,
        client_nonce: "fyko+d2lbbFgONRv9qkxdawL",
        server_nonce: "3rfcNHYJY1ZVvWVs7j",
        salt: "QSXCR+Q6sek8bf92",
        client_first: "n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL",
        server_first: "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096",
        client_final: "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=",
        server_final: "v=rmF9pqV8S7suAoZWja4dJRkFsKQ=",
    },
    Vector {
        mech: Mechanism::ScramSha256,
        client_nonce: "rOprNGfwEbeRWgbNEkqO",
        server_nonce: "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0",
        salt: "W22ZaJ0SNY7soEsUEjb6gQ==",
        client_first: "n,,n=user,r=rOprNGfwEbeRWgbNEkqO",
        server_first: "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
        client_final: "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
        server_final: "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=",
    },
];

fn creds(v: &Vector) -> Credentials {
    Credentials::derive(v.mech, b"pencil", &base64_decode(v.salt.as_bytes()).unwrap(), 4096)
}

#[test]
fn both_sides_reproduce_the_rfc_exchanges() {
    for v in &VECTORS {
        let (mut client, first) = ClientSession::start(v.mech, "user", b"pencil", v.client_nonce);
        assert_eq!(first, v.client_first.as_bytes());
        assert_eq!(client.respond(v.server_first.as_bytes()).unwrap(), v.client_final.as_bytes());
        assert_eq!(client.verify(v.server_final.as_bytes()), Ok(()));
        let cf = ClientFirst::parse(v.client_first.as_bytes()).unwrap();
        assert_eq!(cf.user, "user");
        let (mut server, sf) = ServerSession::start(v.mech, &cf, creds(v), v.server_nonce);
        assert_eq!(sf, v.server_first.as_bytes());
        assert_eq!(server.finish(v.client_final.as_bytes()).unwrap(), v.server_final.as_bytes());
        assert_eq!(server.finish(v.client_final.as_bytes()), Err(ScramError::OutOfOrder));
    }
}

#[test]
fn failures() {
    let v = &VECTORS[1];
    let cf = ClientFirst::parse(v.client_first.as_bytes()).unwrap();
    let server = |c: &str| {
        ServerSession::start(v.mech, &cf, creds(v), v.server_nonce).0.finish(c.as_bytes())
    };
    // A wrong proof, another nonce, a proof of the wrong length, channel binding asked for.
    let wrong = v.client_final.replace("p=dHzb", "p=dHza");
    assert_eq!(server(&wrong), Err(ScramError::AuthenticationFailed));
    let nonce = v.client_final.replace("k0,p=", "k1,p=");
    assert_eq!(server(&nonce), Err(ScramError::AuthenticationFailed));
    assert_eq!(server("c=biws,r=x,p=AAAA"), Err(ScramError::AuthenticationFailed));
    assert!(matches!(server("c=cD10bHMtdW5pcXVlLCw=,r=x,p=AAAA"), Err(ScramError::Malformed(_))));
    for bad in
        ["c=biws,r=x", "x,p=AAAA", "c=biws,p=AAAA", "c=!!,r=x,p=AAAA", "c=biws,r=x,p=!", "\u{ff}"]
    {
        assert!(matches!(server(bad), Err(ScramError::Malformed(_))), "{bad}");
    }
    assert!(matches!(
        ServerSession::start(v.mech, &cf, creds(v), "n").0.finish(&[0xff]),
        Err(ScramError::Malformed(_))
    ));
    for bad in [
        "n,,n=,r=a",
        "p=x,,n=u,r=a",
        "n,,m=x,n=u,r=a",
        "n,,n=u",
        "n,,n=u,r=a,b",
        "n,,n=u=2D,r=a",
        "n,,n=u,r=a b",
        "n,,n=u,x",
        "\u{ff}",
    ] {
        assert!(
            matches!(ClientFirst::parse(bad.as_bytes()), Err(ScramError::Malformed(_))),
            "{bad}"
        );
    }
    assert_eq!(ClientFirst::parse(b"y,,n=a=2Cb=3Dc,r=n").unwrap().user, "a,b=c");
    for bad in ["n,,1=u,r=a", "n,,n=u,r=", "n,,n=u,r=a\u{7f}"] {
        assert!(
            matches!(ClientFirst::parse(bad.as_bytes()), Err(ScramError::Malformed(_))),
            "{bad}"
        );
    }
    // A client that used `y,,`, and a proof of the wrong length with the right nonce.
    let full_nonce = format!("{}{}", v.client_nonce, v.server_nonce);
    assert_eq!(
        server(&format!("c=eSws,r={full_nonce},p=AAAA")),
        Err(ScramError::AuthenticationFailed)
    );
    assert_eq!(
        server(&format!("c=biws,r={full_nonce},p=AAAA")),
        Err(ScramError::AuthenticationFailed)
    );
    let mut c0 = ClientSession::start(v.mech, "u", b"p", "abc").0;
    assert_eq!(c0.respond(b"r=abc d,s=AAAA,i=1"), Err(ScramError::AuthenticationFailed));
    // The client: a server that drops or keeps only the client's nonce, a bad salt or count,
    // a wrong signature, and calls out of order.
    let respond =
        |sf: &str| ClientSession::start(v.mech, "u", b"p", "abc").0.respond(sf.as_bytes());
    assert_eq!(respond("r=xyz1,s=AAAA,i=1"), Err(ScramError::AuthenticationFailed));
    assert_eq!(respond("r=abc,s=AAAA,i=1"), Err(ScramError::AuthenticationFailed));
    for bad in [
        "r=abcd,s=!,i=1",
        "r=abcd,s=AAAA,i=x",
        "r=abcd,s=AAAA,i=0",
        "r=abcd,s=AAAA,i=99999999",
        "r=abcd",
        "\u{ff}",
    ] {
        assert!(matches!(respond(bad), Err(ScramError::Malformed(_))), "{bad}");
    }
    let (mut c, first) = ClientSession::start(v.mech, "a,b=c", b"p", "abc");
    assert_eq!(first, b"n,,n=a=2Cb=3Dc,r=abc");
    assert_eq!(c.verify(b"v=AAAA"), Err(ScramError::OutOfOrder));
    c.respond(b"r=abcd,s=AAAA,i=4096").unwrap();
    assert_eq!(c.respond(b"r=abcd,s=AAAA,i=1"), Err(ScramError::OutOfOrder));
    assert_eq!(c.verify(b"v=AAAA"), Err(ScramError::AuthenticationFailed));
    assert_eq!(c.verify(b"e=other-error"), Err(ScramError::AuthenticationFailed));
    assert!(matches!(c.verify(b"v=!"), Err(ScramError::Malformed(_))));
    assert!(matches!(c.verify(&[0xff]), Err(ScramError::Malformed(_))));
    for e in [ScramError::Malformed("x"), ScramError::AuthenticationFailed, ScramError::OutOfOrder]
    {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn mechanisms_and_refdb_passwords() {
    for m in [Mechanism::ScramSha1, Mechanism::ScramSha256] {
        assert_eq!(Mechanism::from_name(m.name()), Some(m));
    }
    assert_eq!(Mechanism::from_name("PLAIN"), None);
    assert_eq!(
        (Mechanism::ScramSha1.default_iterations(), Mechanism::ScramSha256.default_iterations()),
        (10_000, 15_000)
    );
    assert_eq!((Mechanism::ScramSha1.salt_len(), Mechanism::ScramSha256.salt_len()), (16, 28));
    // SCRAM-SHA-1 hashes the hex MD5 of `user:ref:password` (MD5 of RFC 1321, checked there).
    let p = Mechanism::ScramSha1.prepare_password("user", "pencil", true).unwrap();
    let want: String =
        teseladb_crypto::md5(b"user:ref:pencil").iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(p, want.into_bytes());
    // SCRAM-SHA-256 SASLpreps.
    assert_eq!(Mechanism::ScramSha256.prepare_password("u", "I\u{00AD}X", true).unwrap(), b"IX");
    let e = Mechanism::ScramSha256.prepare_password("u", "\u{0007}", true).unwrap_err();
    assert!(matches!(e, ScramError::Saslprep(_)));
    assert!(!e.to_string().is_empty());
}

/// Found by the 24-hour fuzzing (T010): a proof with a form feed inside its Base64 decoded to the
/// same bytes and logged in. RefDb 9.0.4 refuses a proof, salt, verifier or channel binding
/// with any character outside Base64 (measured: `invalid base64`, answered as
/// `AuthenticationFailed`), and so does RFC 5802.
#[test]
fn base64_with_whitespace_inside_is_refused() {
    for v in &VECTORS {
        for ws in ["\u{c}", " ", "\n", "\t", "\r"] {
            let (head, proof) = v.client_final.rsplit_once(",p=").unwrap();
            let spaced = format!("{head},p={}{ws}{}", &proof[..10], &proof[10..]);
            let cf = ClientFirst::parse(v.client_first.as_bytes()).unwrap();
            let (mut server, _) = ServerSession::start(v.mech, &cf, creds(v), v.server_nonce);
            assert_eq!(server.finish(spaced.as_bytes()), Err(ScramError::Malformed("proof")));
            let cbind = v.client_final.replacen("c=biws", &format!("c=bi{ws}ws"), 1);
            let (mut server, _) = ServerSession::start(v.mech, &cf, creds(v), v.server_nonce);
            assert_eq!(
                server.finish(cbind.as_bytes()),
                Err(ScramError::Malformed("channel binding"))
            );
            let salt = v.server_first.replacen(",s=", &format!(",s={ws}"), 1);
            let (mut client, _) = ClientSession::start(v.mech, "user", b"pencil", v.client_nonce);
            assert_eq!(client.respond(salt.as_bytes()), Err(ScramError::Malformed("salt")));
            let (mut client, _) = ClientSession::start(v.mech, "user", b"pencil", v.client_nonce);
            client.respond(v.server_first.as_bytes()).unwrap();
            let verifier = v.server_final.replacen("v=", &format!("v={ws}"), 1);
            assert_eq!(client.verify(verifier.as_bytes()), Err(ScramError::Malformed("verifier")));
        }
    }
}

/// A client refuses a server that asks for fewer than 4 096 iterations, as the drivers'
/// authentication specification requires (internal audit of 013, pass 3).
#[test]
fn the_client_refuses_fewer_than_4096_iterations() {
    let v = &VECTORS[1];
    for (count, ok) in [(4095, false), (1, false), (4096, true)] {
        let (mut client, _) = ClientSession::start(v.mech, "user", b"pencil", v.client_nonce);
        let first = v.server_first.replace("i=4096", &format!("i={count}"));
        let r = client.respond(first.as_bytes());
        assert_eq!(r.is_ok(), ok, "{count}: {r:?}");
        if !ok {
            assert_eq!(r, Err(ScramError::Malformed("iteration count")));
        }
    }
}
