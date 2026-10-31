//! Authentication commands as RefDb 9.0.4 answers them (013 T019, FR-008; measured on a
//! `refrouter` with `--keyFile` and a `refnode` with TLS and client certificates, 30-10-2026):
//! `saslStart` and `saslContinue` with SCRAM, with and without `skipEmptyExchange` and with
//! Base64 payloads, REFDB-X509 with `authenticate` and `saslStart`, `logout` (absent, DV-014),
//! `speculativeAuthenticate` and `saslSupportedMechs` in `hello`, and the commands refused
//! before authenticating.

mod common;
#[path = "../../teseladb-auth/tests/support/krb_initiator.rs"]
mod krb;

use common::{Mem, doc, field};
use teseladb_auth::sasl::SaslMechanism;
use teseladb_auth::scram::{ClientSession, Credentials, Mechanism};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, User, UserName};
use teseladb_bson::{Document, Value};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_wire::commands::auth::{
    AUTHENTICATE, SASL_CONTINUE, SASL_START, SPECULATIVE_AUTHENTICATE, allowed_unauthenticated,
    is_auth_command,
};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch, dispatch_with};
use teseladb_wire::msg::OpMsg;
use teseladb_x509::pem::{base64_decode, base64_encode};

const ADMIN: &str = "admin";
const PASSWORD: &str = "secret1";
const CERT_USER: &str = "CN=client,OU=KernelUser,O=RefDb,L=New York City,ST=New York,C=US";

fn s(x: &str) -> Value {
    Value::from(x)
}

fn bin(b: &[u8]) -> Value {
    Value::Binary { subtype: 0, bytes: b.to_vec() }
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    }
}

fn scram_user(name: &str, mechs: &[Mechanism]) -> User {
    let mut u = User::new(UserName::new(name, ADMIN));
    for &m in mechs {
        let prepared = m.prepare_password(name, PASSWORD, true).unwrap();
        let creds = Credentials::derive(m, &prepared, &vec![b'0'; m.salt_len()], 4096);
        match m {
            Mechanism::ScramSha1 => u.scram_sha1 = Some(creds),
            Mechanism::ScramSha256 => u.scram_sha256 = Some(creds),
        }
    }
    u
}

/// Stores `user` as Group 0 would.
fn add_user(m: &mut Mem, user: &User) {
    let op = SecurityOp::named(
        Principal::User,
        &user.name.db,
        &user.name.user,
        Expect::Absent,
        Some(user.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
}

/// A node with users `both` (both mechanisms), `sha1` (SCRAM-SHA-1 only) and the certificate's
/// user in `$external`.
fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(NodeSessions::new(Timeouts::default()));
    add_user(&mut m, &scram_user("both", &[Mechanism::ScramSha1, Mechanism::ScramSha256]));
    add_user(&mut m, &scram_user("sha1", &[Mechanism::ScramSha1]));
    add_user(&mut m, &User::new(UserName::new(CERT_USER, EXTERNAL_DB)));
    m
}

/// A connection of a node that requires authentication, with a client certificate or not.
fn conn(cert: bool) -> Connection {
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.peer_user = cert.then(|| CERT_USER.to_owned());
    c
}

fn request(cmd: &Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(1);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

fn run(m: &mut Mem, c: &mut Connection, db: &str, cmd: Document) -> Document {
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(c, &request(&cmd.with("$db", db)), &info(), Some((m, &shared)));
    m.shared = shared;
    match a {
        Action::Reply(mut d) => {
            d.remove("$clusterTime");
            d.remove("operationTime");
            d
        }
        other => panic!("{other:?}"),
    }
}

fn error(code: i32, name: &str, msg: &str) -> Document {
    doc(&[("ok", 0.0.into()), ("errmsg", s(msg)), ("code", code.into()), ("codeName", s(name))])
}

fn conversation(done: bool, payload: Value) -> Document {
    doc(&[
        ("conversationId", 1.into()),
        ("done", done.into()),
        ("payload", payload),
        ("ok", 1.0.into()),
    ])
}

fn payload_bytes(r: &Document) -> Vec<u8> {
    match field(r, "payload") {
        Value::Binary { bytes, .. } => bytes.clone(),
        Value::String(t) => base64_decode(t.as_bytes()).unwrap(),
        other => panic!("{other:?}"),
    }
}

fn authenticated(m: &mut Mem, c: &mut Connection) -> Vec<Value> {
    let r = run(m, c, ADMIN, doc(&[("connectionStatus", 1.into())]));
    let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
    let Value::Array(users) = field(info, "authenticatedUsers") else { panic!("{info:?}") };
    users.clone()
}

/// A `saslStart` of `mech` for `user` with the client's `options`; the client and the reply.
fn start(
    m: &mut Mem,
    c: &mut Connection,
    mech: Mechanism,
    user: &str,
    extra: &[(&str, Value)],
) -> (ClientSession, Document) {
    let prepared = mech.prepare_password(user, PASSWORD, false).unwrap();
    let (client, first) = ClientSession::start(mech, user, &prepared, "clientnonce0123");
    let mut cmd =
        doc(&[(SASL_START, 1.into()), ("mechanism", s(mech.name())), ("payload", bin(&first))]);
    for (k, v) in extra {
        cmd.push(*k, v.clone());
    }
    (client, run(m, c, ADMIN, cmd))
}

fn cont(m: &mut Mem, c: &mut Connection, payload: Value) -> Document {
    let cmd = doc(&[(SASL_CONTINUE, 1.into()), ("conversationId", 1.into()), ("payload", payload)]);
    run(m, c, ADMIN, cmd)
}

#[test]
fn scram_with_and_without_the_empty_exchange() {
    for mech in [Mechanism::ScramSha1, Mechanism::ScramSha256] {
        for skip in [false, true] {
            let (mut m, mut c) = (node(), conn(false));
            let options =
                [("options", Value::Document(doc(&[("skipEmptyExchange", skip.into())])))];
            let (mut client, r) = start(&mut m, &mut c, mech, "both", &options);
            assert_eq!(field(&r, "done"), &Value::Boolean(false));
            let server_first = payload_bytes(&r);
            // RefDb sends 24 random bytes as the server's part of the nonce.
            let nonce_len =
                std::str::from_utf8(&server_first).unwrap().split(',').next().unwrap().len();
            assert_eq!(nonce_len, "r=clientnonce0123".len() + 32);
            let r = cont(&mut m, &mut c, bin(&client.respond(&server_first).unwrap()));
            client.verify(&payload_bytes(&r)).unwrap();
            if skip {
                assert_eq!(field(&r, "done"), &Value::Boolean(true));
            } else {
                assert_eq!(field(&r, "done"), &Value::Boolean(false));
                assert!(authenticated(&mut m, &mut c).is_empty());
                assert_eq!(cont(&mut m, &mut c, bin(b"")), conversation(true, bin(b"")));
            }
            let me = Value::Document(doc(&[("user", s("both")), ("db", s(ADMIN))]));
            assert_eq!(authenticated(&mut m, &mut c), vec![me]);
            let gone = error(17, "ProtocolError", "No SASL session state found");
            assert_eq!(cont(&mut m, &mut c, bin(b"")), gone);
            assert_eq!(c.auth.user.as_ref().map(|u| u.mechanism.name()), Some(mech.name()));
        }
    }
}

#[test]
fn wrong_password_ends_the_conversation() {
    let (mut m, mut c) = (node(), conn(false));
    let prepared = Mechanism::ScramSha256.prepare_password("both", "wrong", false).unwrap();
    let (mut client, first) = ClientSession::start(Mechanism::ScramSha256, "both", &prepared, "n0");
    let cmd =
        doc(&[(SASL_START, 1.into()), ("mechanism", s("SCRAM-SHA-256")), ("payload", bin(&first))]);
    let r = run(&mut m, &mut c, ADMIN, cmd);
    let r = cont(&mut m, &mut c, bin(&client.respond(&payload_bytes(&r)).unwrap()));
    assert_eq!(r, error(18, "AuthenticationFailed", "Authentication failed."));
    let gone = error(17, "ProtocolError", "No SASL session state found");
    assert_eq!(cont(&mut m, &mut c, bin(b"")), gone);
    assert!(authenticated(&mut m, &mut c).is_empty());
}

/// Base64 strings come back as Base64 strings (measured).
#[test]
fn base64_payloads_are_answered_in_base64() {
    let (mut m, mut c) = (node(), conn(false));
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password("both", PASSWORD, false).unwrap();
    let (mut client, first) = ClientSession::start(mech, "both", &prepared, "n0");
    let options = Value::Document(doc(&[("skipEmptyExchange", true.into())]));
    let cmd = doc(&[
        (SASL_START, 1.into()),
        ("mechanism", s(mech.name())),
        ("payload", s(&base64_encode(&first))),
        ("autoAuthorize", 1.into()),
        ("options", options),
    ]);
    let r = run(&mut m, &mut c, ADMIN, cmd);
    assert!(matches!(field(&r, "payload"), Value::String(_)));
    let fin = client.respond(&payload_bytes(&r)).unwrap();
    let r = cont(&mut m, &mut c, s(&base64_encode(&fin)));
    assert!(matches!(field(&r, "payload"), Value::String(_)));
    assert_eq!(field(&r, "done"), &Value::Boolean(true));
}

#[test]
fn sasl_start_errors_as_refdb_gives_them() {
    let (mut m, mut c) = (node(), conn(false));
    let start = |fields: &[(&str, Value)]| {
        let mut d = doc(&[(SASL_START, 1.into())]);
        for (k, v) in fields {
            d.push(*k, v.clone());
        }
        d
    };
    let mech = |n: &str| ("mechanism", s(n));
    let p = |b: &[u8]| ("payload", bin(b));
    let unavailable = |msg: &str| error(334, "MechanismUnavailable", msg);
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    let cases = [
        (
            ADMIN,
            start(&[p(b"x")]),
            error(
                40414,
                "IDLFailedToParse",
                "BSON field 'saslStart.mechanism' is missing but a required field",
            ),
        ),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256")]),
            error(
                40414,
                "IDLFailedToParse",
                "BSON field 'saslStart.payload' is missing but a required field",
            ),
        ),
        (
            ADMIN,
            start(&[("mechanism", 5.into()), p(b"x")]),
            error(
                14,
                "TypeMismatch",
                "BSON field 'saslStart.mechanism' is the wrong type 'int', expected type 'string'",
            ),
        ),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), ("payload", 5.into())]),
            error(2, "BadValue", "Invalid SASLPayload type. Expected Base64 or BinData, got: int"),
        ),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), ("payload", s("n,,n=both,r=abc"))]),
            error(10270, "Location10270", "Failed decoding SASL payload: invalid base64"),
        ),
        (
            // Base64 with a space inside (measured on 9.0.4: refused like any other character).
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), ("payload", s("biwsbj1i b3RoLHI9YWJj"))]),
            error(10270, "Location10270", "Failed decoding SASL payload: invalid base64"),
        ),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), p(b"x"), ("autoAuthorize", s("x"))]),
            error(
                14,
                "TypeMismatch",
                "BSON field 'saslStart.autoAuthorize' is the wrong type 'string', expected types '[long, decimal, double, bool, int]'",
            ),
        ),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), p(b"x"), ("options", 5.into())]),
            error(
                14,
                "TypeMismatch",
                "BSON field 'saslStart.options' is the wrong type 'int', expected type 'object'",
            ),
        ),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), p(b"x"), ("foo", 1.into())]),
            error(40415, "IDLUnknownField", "BSON field 'saslStart.foo' is an unknown field."),
        ),
        (
            ADMIN,
            start(&[mech("FOO"), p(b"x")]),
            unavailable("Received authentication for mechanism FOO which is not known"),
        ),
        (
            ADMIN,
            start(&[mech("PLAIN"), p(b"x")]),
            unavailable("Received authentication for mechanism PLAIN which is not enabled"),
        ),
        (
            EXTERNAL_DB,
            start(&[mech("SCRAM-SHA-256"), p(b"n,,n=both,r=abcdefgh")]),
            unavailable(
                "Unsupported mechanism 'SCRAM-SHA-256' on authentication database '$external'",
            ),
        ),
        (ADMIN, start(&[mech("SCRAM-SHA-256"), p(b"garbage")]), failed.clone()),
        (ADMIN, start(&[mech("SCRAM-SHA-256"), p(b"n,,n=nobody,r=abcdefgh")]), failed.clone()),
        (
            ADMIN,
            start(&[mech("SCRAM-SHA-256"), p(b"n,,n=sha1,r=abcdefgh")]),
            unavailable(
                "Unable to use SCRAM-SHA-256 based authentication for user without any SCRAM-SHA-256 credentials registered",
            ),
        ),
        // REFDB-X509 without a certificate, and on a database other than `$external`.
        (EXTERNAL_DB, start(&[mech("REFDB-X509"), p(b"")]), failed.clone()),
        ("test", start(&[mech("SCRAM-SHA-256"), p(b"n,,n=both,r=abcdefgh")]), failed),
    ];
    for (db, cmd, expected) in cases {
        assert_eq!(run(&mut m, &mut c, db, cmd.clone()), expected, "{cmd:?}");
    }
}

#[test]
fn sasl_continue_errors_as_refdb_gives_them() {
    let (mut m, mut c) = (node(), conn(false));
    let cont = |fields: &[(&str, Value)]| {
        let mut d = doc(&[(SASL_CONTINUE, 1.into())]);
        for (k, v) in fields {
            d.push(*k, v.clone());
        }
        d
    };
    let cases = [
        (
            cont(&[("payload", bin(b"x"))]),
            error(
                40414,
                "IDLFailedToParse",
                "BSON field 'saslContinue.conversationId' is missing but a required field",
            ),
        ),
        (
            cont(&[("conversationId", 1.into())]),
            error(
                40414,
                "IDLFailedToParse",
                "BSON field 'saslContinue.payload' is missing but a required field",
            ),
        ),
        (
            cont(&[("conversationId", s("a")), ("payload", bin(b"x"))]),
            error(
                14,
                "TypeMismatch",
                "BSON field 'saslContinue.conversationId' is the wrong type 'string', expected type 'int'",
            ),
        ),
        (
            cont(&[("conversationId", 1.0.into()), ("payload", bin(b"x"))]),
            error(
                14,
                "TypeMismatch",
                "BSON field 'saslContinue.conversationId' is the wrong type 'double', expected type 'int'",
            ),
        ),
        (
            cont(&[("conversationId", 7.into()), ("payload", bin(b"x"))]),
            error(17, "ProtocolError", "No SASL session state found"),
        ),
    ];
    for (cmd, expected) in cases {
        assert_eq!(run(&mut m, &mut c, ADMIN, cmd.clone()), expected, "{cmd:?}");
    }
    // A conversation survives a `saslContinue` with another id, and garbage ends it.
    start(&mut m, &mut c, Mechanism::ScramSha256, "both", &[]);
    let wrong_id = cont(&[("conversationId", 2.into()), ("payload", bin(b"x"))]);
    assert_eq!(field(&run(&mut m, &mut c, ADMIN, wrong_id), "code"), &Value::Int32(17));
    let garbage = run(
        &mut m,
        &mut c,
        ADMIN,
        cont(&[("conversationId", 1.into()), ("payload", bin(b"garbage"))]),
    );
    assert_eq!(garbage, error(18, "AuthenticationFailed", "Authentication failed."));
}

/// RefDb 9.0 keeps one user per connection: another user's SCRAM fails and the first stays;
/// the same user again succeeds (measured).
#[test]
fn one_user_per_connection() {
    let (mut m, mut c) = (node(), conn(false));
    let skip = [("options", Value::Document(doc(&[("skipEmptyExchange", true.into())])))];
    let login = |m: &mut Mem, c: &mut Connection, user: &str| {
        let (mut client, r) = start(m, c, Mechanism::ScramSha1, user, &skip);
        cont(m, c, bin(&client.respond(&payload_bytes(&r)).unwrap()))
    };
    assert_eq!(field(&login(&mut m, &mut c, "both"), "done"), &Value::Boolean(true));
    assert_eq!(field(&login(&mut m, &mut c, "sha1"), "code"), &Value::Int32(18));
    assert_eq!(field(&login(&mut m, &mut c, "both"), "done"), &Value::Boolean(true));
    // `logout`, deprecated, does not exist (DV-014).
    let r = run(&mut m, &mut c, ADMIN, doc(&[("logout", 1.into())]));
    assert_eq!(r, error(59, "CommandNotFound", "no such cmd: logout"));
    assert_eq!(authenticated(&mut m, &mut c).len(), 1);
    // REFDB-X509 on a connection authenticated with SCRAM fails too.
    c.auth.peer_user = Some(CERT_USER.to_owned());
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    let x509 = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, x509), failed);
    let sasl =
        doc(&[(SASL_START, 1.into()), ("mechanism", s("REFDB-X509")), ("payload", bin(b""))]);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, sasl), failed);
}

/// `connectionStatus` names the user and the roles granted to it; a user dropped since it
/// authenticated is still named, without roles.
#[test]
fn connection_status_shows_the_user_and_its_roles() {
    let (mut m, mut c) = (node(), conn(true));
    let mut cert = User::new(UserName::new(CERT_USER, EXTERNAL_DB));
    cert.roles = vec![RoleName::new("read", "test")];
    m.security.users.clear();
    add_user(&mut m, &cert);
    let x509 = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
    run(&mut m, &mut c, EXTERNAL_DB, x509);
    let status = |m: &mut Mem, c: &mut Connection| {
        let r = run(m, c, ADMIN, doc(&[("connectionStatus", 1.into())]));
        let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
        info.clone()
    };
    let read = Value::Document(doc(&[("role", s("read")), ("db", s("test"))]));
    assert_eq!(field(&status(&mut m, &mut c), "authenticatedUserRoles"), &Value::Array(vec![read]));
    m.security.users.clear();
    let info = status(&mut m, &mut c);
    assert_eq!(field(&info, "authenticatedUserRoles"), &Value::Array(Vec::new()));
    let me = Value::Document(doc(&[("user", s(CERT_USER)), ("db", s(EXTERNAL_DB))]));
    assert_eq!(field(&info, "authenticatedUsers"), &Value::Array(vec![me]));
}

#[test]
fn x509_with_authenticate_and_sasl_start() {
    let ok = doc(&[("dbname", s(EXTERNAL_DB)), ("user", s(CERT_USER)), ("ok", 1.0.into())]);
    let auth = |user: Option<&str>| {
        let mut d = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
        if let Some(u) = user {
            d.push("user", s(u));
        }
        d
    };
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    let (mut m, mut c) = (node(), conn(true));
    assert_eq!(run(&mut m, &mut c, ADMIN, auth(None)), failed);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, auth(Some("CN=other"))), failed);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, auth(None)), ok);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, auth(Some(CERT_USER))), ok);
    assert_eq!(c.auth.user.as_ref().map(|u| u.mechanism), Some(SaslMechanism::X509));
    let mut bad = auth(Some(CERT_USER));
    bad.push("pwd", s("x"));
    let unknown =
        error(40415, "IDLUnknownField", "BSON field 'authenticate.pwd' is an unknown field.");
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, bad), unknown);
    let missing = error(
        40414,
        "IDLFailedToParse",
        "BSON field 'authenticate.mechanism' is missing but a required field",
    );
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, doc(&[(AUTHENTICATE, 1.into())])), missing);
    let wrong = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", 1.into())]);
    assert_eq!(field(&run(&mut m, &mut c, EXTERNAL_DB, wrong), "code"), &Value::Int32(14));
    for mech in ["SCRAM-SHA-1", "PLAIN", "FOO"] {
        let d = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s(mech))]);
        assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, d), failed);
    }
    // Without the certificate's user: `UserNotFound` through `authenticate`, a plain failure
    // through `saslStart`.
    let (mut m, mut c) = (node(), conn(true));
    m.security.users.retain(|d| {
        let db = teseladb_bson::RawDoc::from_bytes(d).ok().and_then(|r| r.get("db"));
        db != Some(teseladb_bson::RawValue::String(EXTERNAL_DB))
    });
    let not_found = error(
        11,
        "UserNotFound",
        &format!("Could not find user \"{CERT_USER}\" for db \"$external\""),
    );
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, auth(None)), not_found);
    let sasl =
        doc(&[(SASL_START, 1.into()), ("mechanism", s("REFDB-X509")), ("payload", bin(b""))]);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, sasl.clone()), failed);
    let (mut m, mut c) = (node(), conn(true));
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, sasl), conversation(true, bin(b"")));
    assert_eq!(authenticated(&mut m, &mut c).len(), 1);
}

#[test]
fn speculative_authentication_in_hello() {
    let (mut m, mut c) = (node(), conn(true));
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password("both", PASSWORD, false).unwrap();
    let (mut client, first) = ClientSession::start(mech, "both", &prepared, "n0");
    let spec = |db: Option<Value>, payload: Value| {
        let mut d =
            doc(&[(SASL_START, 1.into()), ("mechanism", s(mech.name())), ("payload", payload)]);
        if let Some(db) = db {
            d.push("db", db);
        }
        d.push("options", Value::Document(doc(&[("skipEmptyExchange", true.into())])));
        Value::Document(d)
    };
    let hello = |v: Value| {
        doc(&[
            ("hello", 1.into()),
            ("saslSupportedMechs", s("admin.both")),
            (SPECULATIVE_AUTHENTICATE, v),
        ])
    };
    // Left out when it fails: no `db`, a wrong `db` type, an unknown user, an unknown field.
    for v in [
        spec(None, bin(&first)),
        spec(Some(5.into()), bin(&first)),
        spec(Some(s(ADMIN)), bin(b"n,,n=nobody,r=abcdefgh")),
        Value::Document(doc(&[(SASL_START, 1.into()), ("db", s(ADMIN)), ("foo", 1.into())])),
    ] {
        let r = run(&mut m, &mut c, ADMIN, hello(v));
        assert!(r.get(SPECULATIVE_AUTHENTICATE).is_none(), "{r:?}");
    }
    let r = run(&mut m, &mut c, ADMIN, hello(spec(Some(s(ADMIN)), bin(&first))));
    let keys: Vec<&str> = r.iter().map(|(k, _)| k).collect();
    let n = keys.len();
    assert_eq!(keys[n - 3..], ["saslSupportedMechs", SPECULATIVE_AUTHENTICATE, "ok"]);
    let Value::Document(step) = field(&r, SPECULATIVE_AUTHENTICATE) else { panic!("{r:?}") };
    assert_eq!(field(step, "conversationId"), &Value::Int32(1));
    assert_eq!(field(step, "done"), &Value::Boolean(false));
    let r = cont(&mut m, &mut c, bin(&client.respond(&payload_bytes(step)).unwrap()));
    assert_eq!(field(&r, "done"), &Value::Boolean(true));
    assert_eq!(authenticated(&mut m, &mut c).len(), 1);

    // REFDB-X509 answers with the user, and authenticates.
    let (mut m, mut c) = (node(), conn(true));
    let x509 =
        doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509")), ("db", s(EXTERNAL_DB))]);
    let r = run(
        &mut m,
        &mut c,
        ADMIN,
        doc(&[("hello", 1.into()), (SPECULATIVE_AUTHENTICATE, Value::Document(x509))]),
    );
    let expected = doc(&[("dbname", s(EXTERNAL_DB)), ("user", s(CERT_USER))]);
    assert_eq!(field(&r, SPECULATIVE_AUTHENTICATE), &Value::Document(expected));
    assert_eq!(authenticated(&mut m, &mut c).len(), 1);
}

#[test]
fn speculative_authentication_faults_fail_hello() {
    let (mut m, mut c) = (node(), conn(false));
    let hello = |v: Value| doc(&[("hello", 1.into()), (SPECULATIVE_AUTHENTICATE, v)]);
    let cases = [
        (
            1.into(),
            error(
                14,
                "TypeMismatch",
                "BSON field 'hello.speculativeAuthenticate' is the wrong type 'int', expected type 'object'",
            ),
        ),
        (
            Value::Document(Document::new()),
            error(2, "BadValue", "hello.speculativeAuthenticate must be a non-empty Object"),
        ),
        (
            Value::Document(doc(&[("foo", 1.into())])),
            error(51769, "Location51769", "hello.speculativeAuthenticate unknown command: foo"),
        ),
    ];
    for (v, expected) in cases {
        assert_eq!(run(&mut m, &mut c, ADMIN, hello(v)), expected);
    }
}

#[test]
fn sasl_supported_mechs_per_user() {
    let (mut m, mut c) = (node(), conn(false));
    let mechs = |m: &mut Mem, c: &mut Connection, who: Value| {
        let r = run(m, c, ADMIN, doc(&[("hello", 1.into()), ("saslSupportedMechs", who)]));
        field(&r, "saslSupportedMechs").clone()
    };
    let list = |names: &[&str]| Value::Array(names.iter().map(|n| s(n)).collect());
    assert_eq!(mechs(&mut m, &mut c, s("admin.sha1")), list(&["SCRAM-SHA-1"]));
    assert_eq!(mechs(&mut m, &mut c, s("admin.nobody")), list(&["SCRAM-SHA-1", "SCRAM-SHA-256"]));
    assert_eq!(mechs(&mut m, &mut c, s("$external.CN=x")), list(&["REFDB-X509"]));
    let as_doc = Value::Document(doc(&[("user", s("sha1")), ("db", s(ADMIN))]));
    assert_eq!(mechs(&mut m, &mut c, as_doc), list(&["SCRAM-SHA-1"]));
    // Without the node (the handshake alone), the default list.
    let r = match dispatch(
        &mut c,
        &request(&doc(&[
            ("hello", 1.into()),
            ("saslSupportedMechs", s("admin.sha1")),
            ("$db", s(ADMIN)),
        ])),
        &info(),
    ) {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    };
    assert_eq!(field(&r, "saslSupportedMechs"), &list(&["SCRAM-SHA-1", "SCRAM-SHA-256"]));
}

/// Before authenticating, only the handshake, `ping`, `listCommands`, `connectionStatus` and
/// the authentication commands run (measured); unknown commands stay `CommandNotFound`.
#[test]
fn commands_before_authentication() {
    let (mut m, mut c) = (node(), conn(false));
    let ok = |r: &Document| r.get("ok") == Some(&Value::Double(1.0));
    for name in ["ping", "hello", "isMaster", "listCommands", "connectionStatus"] {
        assert!(ok(&run(&mut m, &mut c, ADMIN, doc(&[(name, 1.into())]))), "{name}");
        assert!(allowed_unauthenticated(name));
    }
    for name in ["find", "buildInfo", "insert", "endSessions", "getParameter", "findandmodify"] {
        let r = run(&mut m, &mut c, "test", doc(&[(name, s("c"))]));
        let canonical = if name == "findandmodify" { "findAndModify" } else { name };
        assert_eq!(
            r,
            error(13, "Unauthorized", &format!("Command {canonical} requires authentication"))
        );
    }
    let r = run(&mut m, &mut c, ADMIN, doc(&[("getnonce", 1.into())]));
    assert_eq!(field(&r, "code"), &Value::Int32(59));
    // A node without `--auth` (`--insecure`) runs them all.
    let mut open = Connection::new(2);
    assert!(ok(&run(&mut m, &mut open, "test", doc(&[("find", s("c"))]))));
    assert!(is_auth_command(AUTHENTICATE) && !is_auth_command("logout"));
    assert!(format!("{:?}", c.auth).contains("conversation: false"));
    // Outside a connection's operation nobody is authenticated.
    let r = m.run(ADMIN, doc(&[("connectionStatus", 1.into())]));
    let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
    assert_eq!(field(info, "authenticatedUsers"), &Value::Array(Vec::new()));
}

/// `authenticationRestrictions` of the user and of the roles it inherits decide whether the
/// client may authenticate from its address to the node's; a refusal is `Authentication
/// failed.` and leaves the connection unauthenticated.
#[test]
fn authentication_restrictions_apply_at_login() {
    let restricted = |client: &str| {
        Value::Array(vec![Value::Document(doc(&[("clientSource", Value::Array(vec![s(client)]))]))])
    };
    let mut m = node();
    let mut user = scram_user("both", &[Mechanism::ScramSha1, Mechanism::ScramSha256]);
    let Value::Array(own) = restricted("10.0.0.0/8") else { panic!() };
    user.restrictions = Some(own);
    m.security.users.clear();
    add_user(&mut m, &user);
    let login = |m: &mut Mem, from: &str| {
        let mut c = conn(false);
        c.auth.client_addr = Some(from.to_owned());
        c.auth.server_addr = Some("127.0.0.1:27017".to_owned());
        let skip = [("options", Value::Document(doc(&[("skipEmptyExchange", true.into())])))];
        let (mut client, r) = start(m, &mut c, Mechanism::ScramSha256, "both", &skip);
        let r = cont(m, &mut c, bin(&client.respond(&payload_bytes(&r)).unwrap()));
        (r, c.auth.user.is_some())
    };
    assert!(login(&mut m, "10.1.2.3:5000").1);
    let (r, in_) = login(&mut m, "127.0.0.1:5000");
    assert_eq!(r, error(18, "AuthenticationFailed", "Authentication failed."));
    assert!(!in_);
    // A role's restrictions apply too, and REFDB-X509 is checked the same way.
    let role = doc(&[
        ("createRole", s("ops")),
        ("privileges", Value::Array(vec![])),
        ("roles", Value::Array(vec![])),
        ("authenticationRestrictions", restricted("192.168.0.0/16")),
    ]);
    assert_eq!(field(&m.run(ADMIN, role), "ok"), &Value::Double(1.0));
    let mut cert = User::new(UserName::new(CERT_USER, EXTERNAL_DB));
    cert.roles = vec![RoleName::new("ops", ADMIN)];
    add_user(&mut m, &cert);
    let x509 = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
    for (from, ok) in [("192.168.3.4:1", true), ("10.0.0.1:1", false)] {
        let mut c = conn(true);
        c.auth.client_addr = Some(from.to_owned());
        let r = run(&mut m, &mut c, EXTERNAL_DB, x509.clone());
        assert_eq!(c.auth.user.is_some(), ok, "{from}: {r:?}");
    }
    assert!(format!("{:?}", conn(false).auth).contains("client_addr: None"));
}

#[test]
fn authentications_are_counted_for_server_status() {
    // `security.authentication` of `serverStatus` (006 T046): attempts by mechanism, successes,
    // speculative ones in `hello` and `saslSupportedMechs`.
    let (mut m, mut c) = (node(), conn(true));
    let auth = |user: &str| {
        doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509")), ("user", s(user))])
    };
    run(&mut m, &mut c, EXTERNAL_DB, auth("CN=other"));
    run(&mut m, &mut c, EXTERNAL_DB, auth(CERT_USER));
    let mut c2 = conn(true);
    let x509 =
        doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509")), ("db", s(EXTERNAL_DB))]);
    let hello = doc(&[
        ("hello", 1.into()),
        ("saslSupportedMechs", s("admin.both")),
        (SPECULATIVE_AUTHENTICATE, Value::Document(x509)),
    ]);
    run(&mut m, &mut c2, ADMIN, hello);
    let g = teseladb_wire::commands::lock(&m.shared);
    let x = g.status.mechanisms.get("REFDB-X509").copied().unwrap_or_default();
    assert_eq!(x.authenticate, (2, 1));
    assert_eq!(x.speculative, (1, 1));
    assert_eq!(g.status.sasl_supported_mechs, 1);
}

/// PLAIN (013 FR-025), with `authenticationMechanisms` naming it, as RefDb Enterprise 9.0.4
/// answers it (measured with an OpenLDAP server): on `$external` the LDAP server checks the
/// password and the user needs its `$external` document; on another database the user's SCRAM
/// credentials check it. Every failure is `Authentication failed.`.
#[test]
fn plain_logs_in_against_ldap_or_the_local_credentials() {
    let mut m = node();
    add_user(&mut m, &User::new(UserName::new("jane", EXTERNAL_DB)));
    m.ldap = vec![("jane".into(), b"janepw".to_vec()), ("ghost".into(), b"x".to_vec())];
    let plain = |c: &mut Connection| {
        c.auth.mechanisms = vec![
            teseladb_auth::sasl::SaslMechanism::Plain,
            teseladb_auth::sasl::SaslMechanism::ScramSha256,
        ];
    };
    let start = |payload: &[u8]| {
        doc(&[(SASL_START, 1.into()), ("mechanism", s("PLAIN")), ("payload", bin(payload))])
    };
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    let mut c = conn(false);
    plain(&mut c);
    assert_eq!(
        run(&mut m, &mut c, EXTERNAL_DB, start(b"\0jane\0janepw")),
        conversation(true, bin(b""))
    );
    let me = Value::Document(doc(&[("user", s("jane")), ("db", s(EXTERNAL_DB))]));
    assert_eq!(authenticated(&mut m, &mut c), vec![me]);
    for (db, payload) in [
        (EXTERNAL_DB, &b"\0jane\0wrong"[..]),
        (EXTERNAL_DB, b"\0ghost\0x"),
        (EXTERNAL_DB, b"janepw"),
        (EXTERNAL_DB, b"\0jane\0"),
        (ADMIN, b"\0both\0wrong"),
        (ADMIN, b"\0nobody\0x"),
    ] {
        let mut c = conn(false);
        plain(&mut c);
        assert_eq!(run(&mut m, &mut c, db, start(payload)), failed, "{db} {payload:?}");
    }
    // A local user with its own password.
    let mut c = conn(false);
    plain(&mut c);
    let pw = format!("\0both\0{PASSWORD}");
    assert_eq!(run(&mut m, &mut c, ADMIN, start(pw.as_bytes())), conversation(true, bin(b"")));
    // Without it in `authenticationMechanisms`, PLAIN is not enabled.
    let mut c = conn(false);
    let r = run(&mut m, &mut c, EXTERNAL_DB, start(b"\0jane\0janepw"));
    assert_eq!(r.get("code"), Some(&Value::Int32(334)), "{r:?}");
}

/// PLAIN with LDAP authorization (013 FR-026), as RefDb Enterprise 9.0.4 answers it (measured
/// with `--ldapAuthzQueryTemplate` against OpenLDAP): the user's groups are its roles in `admin`,
/// listed with what they inherit even when no role of that name exists; a user in no group logs
/// in without roles; a document in `$external` is not needed and, if there is one, not read.
#[test]
fn plain_with_ldap_groups_takes_the_groups_as_roles() {
    let readers = RoleName::new("cn=readers,ou=groups,dc=example,dc=com", ADMIN);
    let writers = RoleName::new("cn=writers,ou=groups,dc=example,dc=com", ADMIN);
    let mut m = node();
    let role = teseladb_auth::roles::Role {
        name: readers.clone(),
        privileges: Vec::new(),
        roles: vec![RoleName::new("read", "test")],
        restrictions: None,
    };
    let op = SecurityOp::named(
        Principal::Role,
        ADMIN,
        &readers.role,
        Expect::Absent,
        Some(role.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
    // A document that would give `root`: not read.
    let mut doc_user = User::new(UserName::new("jane", EXTERNAL_DB));
    doc_user.roles = vec![RoleName::new("root", ADMIN)];
    add_user(&mut m, &doc_user);
    m.ldap = vec![("jane".into(), b"janepw".to_vec()), ("lone".into(), b"lonepw".to_vec())];
    m.ldap_groups = Some(vec![("jane".into(), vec![readers.clone(), writers.clone()])]);
    let login = |m: &mut Mem, payload: &[u8]| {
        let mut c = conn(false);
        c.auth.mechanisms = vec![teseladb_auth::sasl::SaslMechanism::Plain];
        let start =
            doc(&[(SASL_START, 1.into()), ("mechanism", s("PLAIN")), ("payload", bin(payload))]);
        assert_eq!(run(m, &mut c, EXTERNAL_DB, start), conversation(true, bin(b"")));
        c
    };
    let roles = |m: &mut Mem, c: &mut Connection| {
        let r = run(m, c, ADMIN, doc(&[("connectionStatus", 1.into())]));
        let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
        field(info, "authenticatedUserRoles").clone()
    };
    let role_value = |r: &RoleName| Value::Document(doc(&[("role", s(&r.role)), ("db", s(&r.db))]));
    let mut jane = login(&mut m, b"\0jane\0janepw");
    let want = [readers.clone(), writers.clone(), RoleName::new("read", "test")];
    assert_eq!(roles(&mut m, &mut jane), Value::Array(want.iter().map(role_value).collect()));
    // What `read` on `test` allows, and not what the document's `root` would.
    let find = doc(&[("find", s("c"))]);
    assert_eq!(field(&run(&mut m, &mut jane, "test", find.clone()), "ok"), &Value::Double(1.0));
    let insert =
        doc(&[("insert", s("c")), ("documents", Value::Array(vec![Value::Document(doc(&[]))]))]);
    let r = run(&mut m, &mut jane, "test", insert);
    assert_eq!(r.get("code"), Some(&Value::Int32(13)), "{r:?}");
    // A user in no group, without a document.
    let mut lone = login(&mut m, b"\0lone\0lonepw");
    assert_eq!(roles(&mut m, &mut lone), Value::Array(Vec::new()));
    let r = run(&mut m, &mut lone, "test", find);
    assert_eq!(r.get("code"), Some(&Value::Int32(13)), "{r:?}");
    // `invalidateUserCache` empties the node's cache of LDAP groups.
    let mut c = Connection::new(3);
    let r = run(&mut m, &mut c, ADMIN, doc(&[("invalidateUserCache", 1.into())]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    assert_eq!(m.invalidations, 1);
}

/// REFDB-AWS (013 FR-025), as RefDb Enterprise 9.0.4 answers it with a test STS behind
/// `awsSTSUrl`: only on `$external`; the server nonce and the STS host first; then the signed
/// request goes to the STS, whose ARN names the user. Every failure is `Authentication failed.`.
#[test]
fn refdb_aws_logs_in_through_the_sts() {
    use teseladb_auth::aws::{self, SigV4};
    let arn = "arn:aws:iam::123456789012:user/alice";
    let mut m = node();
    add_user(&mut m, &User::new(UserName::new(arn, EXTERNAL_DB)));
    let answer = |arn: &str| {
        format!(
            "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Arn>{arn}</Arn></GetCallerIdentityResult></GetCallerIdentityResponse>"
        )
        .into_bytes()
    };
    let enable = |c: &mut Connection| {
        c.auth.mechanisms = vec![
            teseladb_auth::sasl::SaslMechanism::Aws,
            teseladb_auth::sasl::SaslMechanism::ScramSha256,
        ];
    };
    let first = Document::new()
        .with("r", Value::Binary { subtype: 0, bytes: vec![7; 32] })
        .with("p", 110)
        .to_bytes()
        .unwrap();
    let start =
        doc(&[(SASL_START, 1.into()), ("mechanism", s("REFDB-AWS")), ("payload", bin(&first))]);
    // Only on `$external`.
    let mut c = conn(false);
    enable(&mut c);
    let r = run(&mut m, &mut c, ADMIN, start.clone());
    assert_eq!(
        r,
        error(
            334,
            "MechanismUnavailable",
            "Unsupported mechanism 'REFDB-AWS' on authentication database 'admin'"
        )
    );
    let login = |m: &mut Mem, c: &mut Connection| {
        let r = run(m, c, EXTERNAL_DB, start.clone());
        assert_eq!(r.get("done"), Some(&Value::Boolean(false)), "{r:?}");
        let reply = Document::from(teseladb_bson::RawDoc::from_bytes(&payload_bytes(&r)).unwrap());
        assert_eq!(reply.get("h"), Some(&s("sts.test")));
        let Some(Value::Binary { bytes: nonce, .. }) = reply.get("s") else { panic!("{reply:?}") };
        let nonce64 = base64_encode(nonce);
        let date = "20261030T215406Z";
        let headers = [
            ("Content-Length", "43"),
            ("Content-Type", aws::STS_CONTENT_TYPE),
            ("Host", "sts.test"),
            ("X-Amz-Date", date),
            ("X-RefDb-GS2-CB-Flag", "n"),
            ("X-RefDb-Server-Nonce", nonce64.as_str()),
        ];
        let a = aws::authorization(&SigV4 {
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
        let second = Document::new().with("a", a).with("d", date).to_bytes().unwrap();
        let cont = doc(&[
            ("saslContinue", 1.into()),
            ("conversationId", Value::Int32(1)),
            ("payload", bin(&second)),
        ]);
        run(m, c, EXTERNAL_DB, cont)
    };
    m.sts = Some((200, answer(arn)));
    let mut c = conn(false);
    enable(&mut c);
    assert_eq!(login(&mut m, &mut c), conversation(true, bin(b"")));
    let me = Value::Document(doc(&[("user", s(arn)), ("db", s(EXTERNAL_DB))]));
    assert_eq!(authenticated(&mut m, &mut c), vec![me]);
    let sent = m.sts_requests.last().unwrap();
    assert!(sent.headers.iter().any(|(k, v)| k == "X-RefDb-GS2-CB-Flag" && v == "n"));
    // The STS refuses, it names a user that does not exist, or it cannot be reached.
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    for sts in [
        Some((403, b"<ErrorResponse/>".to_vec())),
        Some((200, answer("arn:aws:iam::123456789012:user/bob"))),
        None,
    ] {
        m.sts = sts;
        let mut c = conn(false);
        enable(&mut c);
        assert_eq!(login(&mut m, &mut c), failed);
    }
    // A first message RefDb refuses, and a second that does not parse.
    let mut c = conn(false);
    enable(&mut c);
    let bad =
        doc(&[(SASL_START, 1.into()), ("mechanism", s("REFDB-AWS")), ("payload", bin(b"xyz"))]);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, bad), failed);
    let r = run(&mut m, &mut c, EXTERNAL_DB, start.clone());
    assert_eq!(r.get("done"), Some(&Value::Boolean(false)));
    let cont = |id: i32| {
        doc(&[
            ("saslContinue", 1.into()),
            ("conversationId", Value::Int32(id)),
            ("payload", bin(b"x")),
        ])
    };
    let r = run(&mut m, &mut c, EXTERNAL_DB, cont(2));
    assert_eq!(r.get("code"), Some(&Value::Int32(17)), "{r:?}");
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, cont(1)), failed);
    // A client certificate does not make REFDB-AWS log in.
    let mut c = conn(true);
    enable(&mut c);
    let r = run(&mut m, &mut c, EXTERNAL_DB, start);
    assert_eq!(r.get("done"), Some(&Value::Boolean(false)));
}

/// REFDB-OIDC (013 FR-025), as RefDb Enterprise 9.0.4 answers it with a test provider: one
/// step with the token, or two with the provider's details first; the user is
/// `<authNamePrefix>/<sub>` with the groups of the token as roles, without a document; a key the
/// JWKS does not know makes the node read it again once. Every failure is `Authentication failed.`.
#[test]
fn refdb_oidc_logs_in_with_a_token() {
    use teseladb_crypto::HashAlgorithm;
    let issuer = "https://idp.test";
    let pem = include_str!("../../teseladb-auth/tests/fixtures/oidc/rsa2048.pem");
    let teseladb_x509::PrivateKey::Rsa(key, _) =
        teseladb_x509::PrivateKey::from_pem(pem, None).unwrap()
    else {
        panic!("not RSA")
    };
    let b64url = |b: &[u8]| -> String {
        base64_encode(b).trim_end_matches('=').replace('+', "-").replace('/', "_")
    };
    let p = key.public_key();
    let jwks = format!(
        r#"{{"keys": [{{"kty": "RSA", "kid": "k1", "n": "{}", "e": "{}"}}]}}"#,
        b64url(&p.modulus()),
        b64url(&p.exponent())
    );
    let mut m = node();
    m.oidc = Some((
        format!(
            r#"[{{"issuer": "{issuer}", "audience": "tdb", "authNamePrefix": "idp", "clientId": "cid", "requestScopes": ["s1"], "authorizationClaim": "groups"}}]"#
        ),
        jwks.into_bytes(),
    ));
    let now_s = m.now / 1000;
    let token = |kid: &str, groups: &str| {
        let h = format!(r#"{{"alg":"RS256","kid":"{kid}"}}"#);
        let c = format!(
            r#"{{"iss":"{issuer}","aud":"tdb","sub":"alice","exp":{},"groups":{groups}}}"#,
            now_s + 600
        );
        let signed = format!("{}.{}", b64url(h.as_bytes()), b64url(c.as_bytes()));
        let sig = key.sign_pkcs1v15(HashAlgorithm::Sha256, signed.as_bytes()).unwrap();
        format!("{signed}.{}", b64url(&sig))
    };
    let payload = |d: Document| bin(&d.to_bytes().unwrap());
    let start = |d: Document| {
        doc(&[(SASL_START, 1.into()), ("mechanism", s("REFDB-OIDC")), ("payload", payload(d))])
    };
    let enable = |c: &mut Connection| {
        c.auth.mechanisms = vec![teseladb_auth::sasl::SaslMechanism::Oidc];
    };
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    // One step.
    let mut c = conn(false);
    enable(&mut c);
    let jwt = Document::new().with("jwt", token("k1", r#"["readers"]"#));
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, start(jwt.clone())), conversation(true, bin(b"")));
    let r = run(&mut m, &mut c, ADMIN, doc(&[("connectionStatus", 1.into())]));
    let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
    let user = Value::Document(doc(&[("user", s("idp/alice")), ("db", s(EXTERNAL_DB))]));
    assert_eq!(field(info, "authenticatedUsers"), &Value::Array(vec![user]));
    let role = Value::Document(doc(&[("role", s("idp/readers")), ("db", s(ADMIN))]));
    assert_eq!(field(info, "authenticatedUserRoles"), &Value::Array(vec![role]));
    assert_eq!(m.jwks_reads, vec![false]);
    // Two steps.
    let mut c = conn(false);
    enable(&mut c);
    let r = run(&mut m, &mut c, EXTERNAL_DB, start(Document::new().with("n", "alice")));
    let reply = Document::from(teseladb_bson::RawDoc::from_bytes(&payload_bytes(&r)).unwrap());
    assert_eq!(reply.get("issuer"), Some(&s(issuer)));
    assert_eq!(reply.get("clientId"), Some(&s("cid")));
    let cont = |d: Document, id: i32| {
        doc(&[
            ("saslContinue", 1.into()),
            ("conversationId", Value::Int32(id)),
            ("payload", payload(d)),
        ])
    };
    let r = run(&mut m, &mut c, EXTERNAL_DB, cont(jwt.clone(), 2));
    assert_eq!(r.get("code"), Some(&Value::Int32(17)), "{r:?}");
    assert_eq!(
        run(&mut m, &mut c, EXTERNAL_DB, cont(jwt.clone(), 1)),
        conversation(true, bin(b""))
    );
    // A second message without the token.
    let mut c = conn(false);
    enable(&mut c);
    run(&mut m, &mut c, EXTERNAL_DB, start(Document::new().with("n", "alice")));
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, cont(Document::new().with("n", "x"), 1)), failed);
    // A second message that is not BSON.
    let mut c = conn(false);
    enable(&mut c);
    run(&mut m, &mut c, EXTERNAL_DB, start(Document::new().with("n", "alice")));
    let garbage = doc(&[
        ("saslContinue", 1.into()),
        ("conversationId", Value::Int32(1)),
        ("payload", bin(b"x")),
    ]);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, garbage), failed);
    // A key the JWKS does not know: read again once, then refused.
    m.jwks_reads.clear();
    let mut c = conn(false);
    enable(&mut c);
    let unknown = Document::new().with("jwt", token("k9", "[]"));
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, start(unknown)), failed);
    assert_eq!(m.jwks_reads, vec![false, true]);
    // Bad tokens and messages, another database, no provider.
    for bad in [
        Document::new().with("jwt", "a.b.c"),
        Document::new().with("jwt", token("k1", "1")),
        Document::new().with("z", 1),
    ] {
        let mut c = conn(false);
        enable(&mut c);
        assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, start(bad)), failed);
    }
    let mut c = conn(false);
    enable(&mut c);
    let r = run(&mut m, &mut c, ADMIN, start(jwt.clone()));
    assert_eq!(r.get("code"), Some(&Value::Int32(334)), "{r:?}");
    m.oidc = None;
    let mut c = conn(false);
    enable(&mut c);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, start(jwt)), failed);
}

#[test]
fn gssapi_logs_in_with_a_kerberos_ticket() {
    let mut m = node();
    m.now = u64::try_from(krb::NOW).unwrap() * 1000;
    m.kerberos = krb::keytab();
    let user = "alice@TESELA.TEST";
    add_user(&mut m, &User::new(UserName::new(user, EXTERNAL_DB)));
    let enable = |c: &mut Connection| {
        c.auth.mechanisms = vec![SaslMechanism::Gssapi, SaslMechanism::ScramSha256];
    };
    let start = |o: &krb::Options| {
        let token = krb::ap_req(o);
        doc(&[(SASL_START, 1.into()), ("mechanism", s("GSSAPI")), ("payload", bin(&token))])
    };
    let next = |p: &[u8]| {
        doc(&[(SASL_CONTINUE, 1.into()), ("conversationId", 1.into()), ("payload", bin(p))])
    };
    let failed = error(18, "AuthenticationFailed", "Authentication failed.");
    // Only on `$external` (measured: 334 on `admin`).
    let mut c = conn(false);
    enable(&mut c);
    assert_eq!(
        run(&mut m, &mut c, ADMIN, start(&krb::options())),
        error(
            334,
            "MechanismUnavailable",
            "Unsupported mechanism 'GSSAPI' on authentication database 'admin'"
        )
    );
    let answer = [&[1u8, 0, 0, 0][..], user.as_bytes()].concat();
    // With mutual authentication: the AP-REP, then the offer after the empty answer, then done.
    for sealed in [false, true] {
        let mut c = conn(false);
        enable(&mut c);
        let r = run(&mut m, &mut c, EXTERNAL_DB, start(&krb::options()));
        assert_eq!(r.get("done"), Some(&Value::Boolean(false)), "{r:?}");
        assert_eq!(&payload_bytes(&r)[..1], &[0x60]);
        let r = run(&mut m, &mut c, EXTERNAL_DB, next(b""));
        assert_eq!(&payload_bytes(&r)[..4], &[0x05, 0x04, 0x01, 0xff]);
        let r = run(&mut m, &mut c, EXTERNAL_DB, next(&krb::initiator_wrap(&answer, sealed)));
        assert_eq!(r, conversation(true, bin(b"")));
        let r = run(&mut m, &mut c, ADMIN, doc(&[("connectionStatus", 1.into())]));
        let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
        let me = Value::Document(doc(&[("user", s(user)), ("db", s(EXTERNAL_DB))]));
        assert_eq!(field(info, "authenticatedUsers"), &Value::Array(vec![me]));
    }
    // Without it, the offer at once.
    let mut c = conn(false);
    enable(&mut c);
    let o = krb::Options { mutual: false, ..krb::options() };
    let r = run(&mut m, &mut c, EXTERNAL_DB, start(&o));
    assert_eq!(&payload_bytes(&r)[..4], &[0x05, 0x04, 0x01, 0xff]);
    assert_eq!(
        run(&mut m, &mut c, EXTERNAL_DB, next(&krb::initiator_wrap(&answer, true))),
        conversation(true, bin(b""))
    );
    // A ticket that does not open, an answer naming someone else, a user without a document,
    // a wrong conversation and a continue without a conversation.
    let mut c = conn(false);
    enable(&mut c);
    let mut bad = krb::ap_req(&krb::options());
    let last = bad.len() - 1;
    bad[last] ^= 1;
    let cmd = doc(&[(SASL_START, 1.into()), ("mechanism", s("GSSAPI")), ("payload", bin(&bad))]);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, cmd), failed);
    let mut c = conn(false);
    enable(&mut c);
    run(&mut m, &mut c, EXTERNAL_DB, start(&o));
    let other = krb::initiator_wrap(b"\x01\0\0\0bob@TESELA.TEST", true);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, next(&other)), failed);
    let mut c = conn(false);
    enable(&mut c);
    run(&mut m, &mut c, EXTERNAL_DB, start(&o));
    let wrong_id =
        doc(&[(SASL_CONTINUE, 1.into()), ("conversationId", 2.into()), ("payload", bin(b""))]);
    let no_state = error(17, "ProtocolError", "No SASL session state found");
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, wrong_id), no_state);
    m.security = Default::default();
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, next(&krb::initiator_wrap(&answer, true))), failed);
    let mut c = conn(false);
    enable(&mut c);
    assert_eq!(run(&mut m, &mut c, EXTERNAL_DB, next(b"")), no_state);
}

/// REFDB-X509 when the node authorizes by LDAP groups (013 T045, FR-026, FR-033), as RefDb
/// Enterprise 9.0.4 answers it (measured with OpenLDAP, `--ldapAuthzQueryTemplate` and a user to
/// DN mapping of the certificate's subject): the groups of the certificate's user are its roles,
/// its document in `$external` is not read and not needed, and `useInternalAuthzForX509` changes
/// none of it, also set at startup.
#[test]
fn x509_under_ldap_authorization_takes_the_groups() {
    let readers = RoleName::new("cn=readers,ou=groups,dc=example,dc=com", ADMIN);
    let mut m = node();
    // The certificate user's document would give `root`.
    let mut doc_user = User::new(UserName::new(CERT_USER, EXTERNAL_DB));
    doc_user.roles = vec![RoleName::new("root", ADMIN)];
    m.security
        .apply(&[SecurityOp::named(
            Principal::User,
            EXTERNAL_DB,
            CERT_USER,
            Expect::Present,
            Some(doc_user.to_document().to_bytes().unwrap()),
        )])
        .unwrap();
    m.ldap_groups = Some(vec![(CERT_USER.into(), vec![readers.clone()])]);
    let roles = |m: &mut Mem, c: &mut Connection| {
        let r = run(m, c, ADMIN, doc(&[("connectionStatus", 1.into())]));
        let Value::Document(info) = field(&r, "authInfo") else { panic!("{r:?}") };
        field(info, "authenticatedUserRoles").clone()
    };
    let readers_value =
        Value::Array(vec![Value::Document(doc(&[("role", s(&readers.role)), ("db", s(ADMIN))]))]);
    for internal in [false, true] {
        let set = doc(&[("setParameter", 1.into()), ("useInternalAuthzForX509", internal.into())]);
        let mut admin = Connection::new(3);
        assert_eq!(field(&run(&mut m, &mut admin, ADMIN, set), "ok"), &Value::Double(1.0));
        // With `authenticate` and with `saslStart`.
        let mut c = conn(true);
        let auth = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
        assert_eq!(field(&run(&mut m, &mut c, EXTERNAL_DB, auth), "ok"), &Value::Double(1.0));
        assert_eq!(roles(&mut m, &mut c), readers_value, "useInternalAuthzForX509 {internal}");
        let mut c = conn(true);
        let start =
            doc(&[(SASL_START, 1.into()), ("mechanism", s("REFDB-X509")), ("payload", bin(b""))]);
        assert_eq!(field(&run(&mut m, &mut c, EXTERNAL_DB, start), "done"), &Value::Boolean(true));
        assert_eq!(roles(&mut m, &mut c), readers_value);
    }
    // A certificate whose user has no document logs in, in no group.
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.peer_user = Some("CN=nobody,OU=people,O=example".into());
    let auth = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
    assert_eq!(field(&run(&mut m, &mut c, EXTERNAL_DB, auth), "ok"), &Value::Double(1.0));
    assert_eq!(roles(&mut m, &mut c), Value::Array(Vec::new()));
    // Without LDAP authorization it needs its document, as before.
    m.ldap_groups = None;
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.peer_user = Some("CN=nobody,OU=people,O=example".into());
    let auth = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
    assert_eq!(field(&run(&mut m, &mut c, EXTERNAL_DB, auth), "code"), &Value::Int32(11));
}

/// `useInternalAuthzForX509` (013 T045), as `refnode` 9.0.4 keeps it (measured): false by
/// default, settable at runtime and at startup, and any value read with BSON truthiness, a
/// string being true.
#[test]
fn use_internal_authz_for_x509_is_a_server_parameter() {
    const P: &str = "useInternalAuthzForX509";
    let mut m = node();
    let mut c = Connection::new(3);
    let get = |m: &mut Mem, c: &mut Connection| {
        field(&run(m, c, ADMIN, doc(&[("getParameter", 1.into()), (P, 1.into())])), P).clone()
    };
    assert_eq!(get(&mut m, &mut c), Value::Boolean(false));
    let details = doc(&[
        ("getParameter", Value::Document(doc(&[("showDetails", true.into())]))),
        (P, 1.into()),
    ]);
    let r = run(&mut m, &mut c, ADMIN, details);
    assert_eq!(field(&r, "useInternalAuthzForX509.settableAtRuntime"), &Value::Boolean(true));
    assert_eq!(field(&r, "useInternalAuthzForX509.settableAtStartup"), &Value::Boolean(true));
    let cases = [
        (s("false"), true, false),
        (s(""), true, true),
        (Value::Null, false, true),
        (2.into(), true, false),
        (0.0.into(), false, true),
        (Value::Document(doc(&[])), true, false),
    ];
    for (v, now, was) in cases {
        let r = run(&mut m, &mut c, ADMIN, doc(&[("setParameter", 1.into()), (P, v.clone())]));
        assert_eq!(field(&r, "was"), &Value::Boolean(was), "{v:?}");
        assert_eq!(get(&mut m, &mut c), Value::Boolean(now), "{v:?}");
    }
}

/// A member's certificate (013 T046), as `refnode` 9.0.4 takes it (measured): with
/// `clusterAuthMode` `sendX509` or `x509` it logs in as `__system`; otherwise it is an ordinary
/// REFDB-X509 user.
#[test]
fn a_members_certificate_logs_in_as_system_with_x509_cluster_authentication() {
    use teseladb_wire::commands::server_params::ClusterAuthMode;
    let mut m = node();
    let auth = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s("REFDB-X509"))]);
    for (mode, internal) in [
        (ClusterAuthMode::Undefined, false),
        (ClusterAuthMode::KeyFile, false),
        (ClusterAuthMode::SendKeyFile, false),
        (ClusterAuthMode::SendX509, true),
        (ClusterAuthMode::X509, true),
    ] {
        m.shared.lock().unwrap().params.cluster_auth_mode = mode;
        let mut c = conn(true);
        c.auth.peer_member = true;
        let r = run(&mut m, &mut c, EXTERNAL_DB, auth.clone());
        // The answer names the certificate's user in every mode.
        assert_eq!(field(&r, "user"), &s(CERT_USER), "{mode:?} {r:?}");
        let who = if internal {
            doc(&[("user", s("__system")), ("db", s("local"))])
        } else {
            doc(&[("user", s(CERT_USER)), ("db", s(EXTERNAL_DB))])
        };
        assert_eq!(authenticated(&mut m, &mut c), vec![Value::Document(who)], "{mode:?}");
    }
    // A certificate that is not a member's stays its own user.
    let mut c = conn(true);
    let r = run(&mut m, &mut c, EXTERNAL_DB, auth);
    assert_eq!(field(&r, "user"), &s(CERT_USER));
}

/// `clusterAuthMode` (013 T046), as `refnode` 9.0.4 keeps it (measured): `undefined` without a
/// key file or the option, settable only at runtime, one step at a time, and the modes past
/// `keyFile` only with TLS.
#[test]
fn cluster_auth_mode_moves_one_step_at_a_time() {
    const P: &str = "clusterAuthMode";
    let mut m = node();
    let mut c = Connection::new(3);
    let get = |m: &mut Mem, c: &mut Connection| {
        field(&run(m, c, ADMIN, doc(&[("getParameter", 1.into()), (P, 1.into())])), P).clone()
    };
    let set = |m: &mut Mem, c: &mut Connection, v: Value| {
        run(m, c, ADMIN, doc(&[("setParameter", 1.into()), (P, v)]))
    };
    assert_eq!(get(&mut m, &mut c), s("undefined"));
    let details = doc(&[
        ("getParameter", Value::Document(doc(&[("showDetails", true.into())]))),
        (P, 1.into()),
    ]);
    let r = run(&mut m, &mut c, ADMIN, details);
    assert_eq!(field(&r, "clusterAuthMode.settableAtStartup"), &Value::Boolean(false));
    let r = set(&mut m, &mut c, s("bogus"));
    assert_eq!(r, error(2, "BadValue", "Invalid clusterAuthMode 'bogus'"));
    let r = set(&mut m, &mut c, s("sendX509"));
    let msg = "Illegal state transition for clusterAuthMode from 'undefined' to 'sendX509'";
    assert_eq!(r, error(5579202, "Location5579202", msg));
    for (to, was) in [
        ("keyFile", "undefined"),
        ("sendKeyFile", "keyFile"),
        ("sendX509", "sendKeyFile"),
        ("x509", "sendX509"),
    ] {
        let r = set(&mut m, &mut c, s(to));
        assert_eq!(field(&r, "was"), &s(was), "{r:?}");
        assert_eq!(get(&mut m, &mut c), s(to));
    }
    let r = set(&mut m, &mut c, s("x509"));
    assert_eq!(
        r,
        error(
            5579202,
            "Location5579202",
            "Illegal state transition for clusterAuthMode from 'x509' to 'x509'"
        )
    );
    // Without TLS only `keyFile` can be reached.
    let mut m = node();
    m.shared.lock().unwrap().params.tls_enabled = false;
    let r = set(&mut m, &mut c, s("sendKeyFile"));
    let msg =
        "Illegal state transition for clusterAuthMode, need to enable SSL for outgoing connections";
    assert_eq!(r, error(2, "BadValue", msg));
    assert_eq!(field(&set(&mut m, &mut c, s("keyFile")), "was"), &s("undefined"));
}
