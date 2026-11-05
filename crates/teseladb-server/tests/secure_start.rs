//! The secure start (013 T015, SC-001, FR-004, FR-007): `teseladb start` without `--insecure`
//! creates the PKI and the `root` credentials, serves the clients' port with TLS only, asks every
//! command but the handshake for authentication, lets `root` in with SCRAM-SHA-256 and the
//! password of the credentials file, and on a restart loads the same PKI and keeps one `root`.

mod started;

use started::*;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use teseladb_auth::scram::Mechanism;
use teseladb_server::pki::{
    CA_CERT, CA_KEY, MASTER_KEY, NODE_CERT, NODE_KEY, ROOT_CREDENTIALS, renew_node,
};
use teseladb_server::run;
use teseladb_wire::msg::{HEADER_LEN, OpMsg};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_tls::Trust;
use teseladb_x509::Certificate;
use teseladb_x509::ext::GeneralName;

#[test]
fn a_start_without_options_is_secure() {
    let d = dir("first");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let node = start(&data, &log);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("PKI created in"), "{text}");
    assert!(!text.contains("insecure mode"), "{text}");
    let mode = std::fs::metadata(data.join(ROOT_CREDENTIALS)).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let (user, password) = credentials(&data);
    assert!(!text.contains(&password), "the log must not show the password");
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    // Plain bytes get no answer.
    let mut plain = TcpStream::connect(("127.0.0.1", node.port)).unwrap();
    plain.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let ping = Document::new().with("ping", 1).with("$db", "admin");
    plain.write_all(&OpMsg::encode(1, 0, 0, &ping.to_bytes().unwrap())).unwrap();
    let mut b = [0u8; 64];
    assert!(!matches!(plain.read(&mut b), Ok(n) if n >= HEADER_LEN));
    // With TLS: the handshake answers, a command asks for authentication.
    let mut c = Client::new(node.port, &ca);
    assert!(ok(&c.command("admin", Document::new().with("ping", 1))));
    let r = c.command("test", Document::new().with("find", "c"));
    assert_eq!(r.get("code"), Some(&Value::Int32(13)));
    // `root` comes in, as soon as core 0 created it.
    let until = Instant::now() + READY_WAIT;
    loop {
        let r = login(&mut c, &user, &password);
        if ok(&r) {
            break;
        }
        assert!(Instant::now() < until, "root never came: {r:?}");
        std::thread::sleep(Duration::from_millis(200));
        c = Client::new(node.port, &ca);
    }
    let r = c.command("test", Document::new().with("find", "c"));
    assert!(ok(&r), "{r:?}");
    let r = c.command("admin", Document::new().with("usersInfo", 1));
    let Some(Value::Array(users)) = r.get("users") else { panic!("{r:?}") };
    assert_eq!(users.len(), 1);
    let r = login(&mut Client::new(node.port, &ca), &user, "not the password");
    assert_eq!(r.get("code"), Some(&Value::Int32(18)));
    let signed = signed_time(&mut c);
    check_signatures(&mut c, &signed);
    drop(node);
    // A restart loads the same PKI and creates no second `root`.
    let node = start(&data, &log);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("PKI loaded from"), "{text}");
    let mut c = Client::new(node.port, &ca);
    let until = Instant::now() + READY_WAIT;
    while !ok(&login(&mut c, &user, &password)) {
        assert!(Instant::now() < until, "root does not log in after a restart");
        std::thread::sleep(Duration::from_millis(200));
        c = Client::new(node.port, &ca);
    }
    let r = c.command("admin", Document::new().with("usersInfo", 1));
    let Some(Value::Array(users)) = r.get("users") else { panic!("{r:?}") };
    assert_eq!(users.len(), 1);
    // The keys are the same after the restart: the time signed before passes.
    let again = signed_time(&mut c);
    assert_eq!(key_id(&again), key_id(&signed));
    check_signatures(&mut c, &signed);
}

/// The `$clusterTime` of a `ping`, once the leader has made the signing keys (013 FR-014).
fn signed_time(c: &mut Client) -> Document {
    let until = Instant::now() + READY_WAIT;
    loop {
        let r = c.command("admin", Document::new().with("ping", 1));
        let Some(Value::Document(ct)) = r.get("$clusterTime") else { panic!("{r:?}") };
        if key_id(ct) != 0 {
            assert_ne!(signature_hash(ct), vec![0; 20]);
            return ct.clone();
        }
        assert!(Instant::now() < until, "no signing key: {r:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn signature(ct: &Document) -> &Document {
    match ct.get("signature") {
        Some(Value::Document(s)) => s,
        other => panic!("{other:?}"),
    }
}

fn key_id(ct: &Document) -> i64 {
    match signature(ct).get("keyId") {
        Some(Value::Int64(id)) => *id,
        other => panic!("{other:?}"),
    }
}

fn signature_hash(ct: &Document) -> Vec<u8> {
    bytes(signature(ct), "hash")
}

/// A signed time the node gave passes; the same signature on a later time does not.
fn check_signatures(c: &mut Client, signed: &Document) {
    let r =
        c.command("admin", Document::new().with("ping", 1).with("$clusterTime", signed.clone()));
    assert!(ok(&r), "{r:?}");
    let Some(Value::Timestamp { time, increment }) = signed.get("clusterTime") else {
        panic!("{signed:?}")
    };
    let later = Document::new()
        .with("clusterTime", Value::Timestamp { time: time + 3600, increment: *increment })
        .with("signature", signature(signed).clone());
    let r = c.command("admin", Document::new().with("ping", 1).with("$clusterTime", later));
    assert_eq!(r.get("code"), Some(&Value::Int32(204)), "{r:?}");
}

#[test]
fn a_secure_start_stops_on_a_broken_pki_or_tls() {
    let d = dir("broken");
    let data = d.0.join("data");
    let base = |extra: &[&str]| {
        let mut a = vec![
            "start",
            "--cores",
            "1",
            "--port",
            "0",
            "--peer-listen",
            "127.0.0.1:0",
            "--admin-listen",
            "127.0.0.1:0",
            // Several nodes on one machine: none opens the HTTP port of 019.
            "--http-port",
            "0",
        ];
        a.extend(["--dbpath", data.to_str().unwrap()]);
        a.extend_from_slice(extra);
        a.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    };
    // A missing piece of an existing PKI.
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("master.key"), "AAAA\n").unwrap();
    let args = base(&[]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, err) = run_once(&refs);
    assert_eq!(code, 1);
    assert!(err.contains("master.key"), "{err}");
    std::fs::remove_dir_all(&data).unwrap();
    // A CA file that does not exist.
    let args =
        base(&["--tlsCAFile", "/nonexistent/ca.pem", "--bind_ip", "127.0.0.1,localhost,node.test"]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, err) = run_once(&refs);
    assert_eq!(code, 1);
    assert!(err.contains("--tlsCAFile"), "{err}");
    // A credentials file that cannot be read.
    std::fs::remove_file(data.join(ROOT_CREDENTIALS)).unwrap();
    std::fs::create_dir(data.join(ROOT_CREDENTIALS)).unwrap();
    let args = base(&[]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, err) = run_once(&refs);
    assert_eq!(code, 1);
    assert!(err.contains("PKI"), "{err}");
}

/// The DER of the certificate a new connection gets.
fn served_leaf(port: u16, ca: &Certificate) -> Vec<u8> {
    let mut c = Client::new(port, ca);
    assert!(ok(&c.command("admin", Document::new().with("ping", 1))));
    c.tls.peer_certificates()[0].der().to_vec()
}

fn disk_leaf(data: &Path) -> Vec<u8> {
    let pem = std::fs::read_to_string(data.join(NODE_CERT)).unwrap();
    Certificate::from_pem_all(&pem).unwrap()[0].der().to_vec()
}

#[test]
fn rotate_certificates_serves_a_renewed_certificate_without_a_restart() {
    let d = dir("rotate");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let node = start(&data, &log);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut c = root_client(node.port, &ca, &data);
    let first = served_leaf(node.port, &ca);
    assert_eq!(first, disk_leaf(&data));
    // Without authentication the command is refused.
    let r =
        Client::new(node.port, &ca).command("admin", Document::new().with("rotateCertificates", 1));
    assert_eq!(r.get("code"), Some(&Value::Int32(13)));
    // A renewal on disk is served only after the rotation.
    let names = vec![GeneralName::Dns("localhost".to_owned()), GeneralName::Ip(vec![127, 0, 0, 1])];
    let now = now_ms() / 1000;
    renew_node(&data, names, now, &mut |b| teseladb_sys::fill_random(b)).unwrap();
    assert_ne!(disk_leaf(&data), first);
    assert_eq!(served_leaf(node.port, &ca), first);
    let rotate = Document::new().with("rotateCertificates", 1).with("message", "renewal");
    let r = c.command("admin", rotate.clone());
    assert!(ok(&r), "{r:?}");
    let renewed = disk_leaf(&data);
    let now_served = served_leaf(node.port, &ca);
    assert!(now_served != first, "still the first certificate after {r:?}");
    assert_eq!(now_served, renewed);
    // The connection open before the rotation keeps working.
    assert!(ok(&c.command("admin", Document::new().with("ping", 1))));
    // The line, as `getLog` shows it (the process's log: other nodes of this test binary may be
    // the ones that write it).
    let until = Instant::now() + READY_WAIT;
    loop {
        let r = c.command("admin", Document::new().with("getLog", "global"));
        if format!("{r:?}").contains("Certificate rotation completed successfully") {
            break;
        }
        assert!(Instant::now() < until, "the rotation was not logged: {r:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A broken file: 140 and the renewed certificate stays.
    let good = std::fs::read(data.join(NODE_CERT)).unwrap();
    std::fs::write(data.join(NODE_CERT), b"garbage").unwrap();
    let r = c.command("admin", rotate);
    assert_eq!(r.get("code"), Some(&Value::Int32(140)), "{r:?}");
    assert_eq!(r.get("codeName"), Some(&Value::from("InvalidSSLConfiguration")));
    assert_eq!(served_leaf(node.port, &ca), renewed);
    std::fs::write(data.join(NODE_CERT), good).unwrap();
    drop(node);
}

#[test]
fn scram_sha_1_and_x509_log_in_end_to_end() {
    use teseladb_x509::oid;
    let d = dir("mechanisms");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    // Client certificates of the cluster's CA, asked for with `--tlsCAFile` as in RefDb.
    drop(start(&data, &log));
    let ca_file = data.join(CA_CERT);
    let tls_ca =
        ["--tlsCAFile", ca_file.to_str().unwrap(), "--tlsAllowConnectionsWithoutCertificates"];
    let node = start_with(&data, &log, &tls_ca);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut root = root_client(node.port, &ca, &data);
    let subject = "CN=compat-x509,OU=compat,O=teseladb";
    let read_test = Value::Array(vec![Value::Document(
        Document::new().with("role", "read").with("db", "test"),
    )]);
    let users = [
        Document::new()
            .with("createUser", "old")
            .with("pwd", "old-pw")
            .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-1")]))
            .with("roles", read_test.clone()),
        Document::new().with("createUser", subject).with("roles", read_test),
    ];
    for (db, u) in [("admin", users[0].clone()), ("$external", users[1].clone())] {
        let r = root.command(db, u);
        assert!(ok(&r), "{r:?}");
    }
    // SCRAM-SHA-1, and the mechanisms the node names for that user.
    let mut c = Client::new(node.port, &ca);
    let mechs = c
        .command("admin", Document::new().with("hello", 1).with("saslSupportedMechs", "admin.old"));
    assert_eq!(
        mechs.get("saslSupportedMechs"),
        Some(&Value::Array(vec![Value::from("SCRAM-SHA-1")]))
    );
    assert!(ok(&login_with(&mut c, Mechanism::ScramSha1, "old", "old-pw")));
    assert!(ok(&c.command("test", Document::new().with("find", "c"))));
    // A mechanism the user has no credentials for: `MechanismUnavailable`, as in RefDb.
    let r = login_with(&mut Client::new(node.port, &ca), Mechanism::ScramSha256, "old", "old-pw");
    assert_eq!(r.get("code"), Some(&Value::Int32(334)));
    // REFDB-X509 with the certificate of the TLS handshake, by `authenticate` and in `hello`.
    let name: [(&[u8], &str); 3] = [
        (oid::ORGANIZATION, "teseladb"),
        (oid::ORGANIZATIONAL_UNIT, "compat"),
        (oid::COMMON_NAME, "compat-x509"),
    ];
    let x509 = Document::new().with("authenticate", 1).with("mechanism", "REFDB-X509");
    let mut c = Client::with_identity(node.port, &ca, Some(client_identity(&data, &name)));
    let r = c.command("$external", x509.clone());
    assert_eq!(r.get("user"), Some(&Value::from(subject)), "{r:?}");
    assert!(ok(&c.command("test", Document::new().with("find", "c"))));
    let mut c = Client::with_identity(node.port, &ca, Some(client_identity(&data, &name)));
    let spec = x509.clone().with("db", "$external");
    let r =
        c.command("admin", Document::new().with("hello", 1).with("speculativeAuthenticate", spec));
    let Some(Value::Document(sa)) = r.get("speculativeAuthenticate") else { panic!("{r:?}") };
    assert_eq!(sa.get("user"), Some(&Value::from(subject)));
    assert!(ok(&c.command("test", Document::new().with("find", "c"))));
    // Without a certificate, or with one of another subject: failure.
    let r = Client::new(node.port, &ca).command("$external", x509.clone());
    assert_eq!(r.get("code"), Some(&Value::Int32(18)), "{r:?}");
    let other: [(&[u8], &str); 1] = [(oid::COMMON_NAME, "nobody")];
    let mut c = Client::with_identity(node.port, &ca, Some(client_identity(&data, &other)));
    let r = c.command("$external", x509);
    assert_eq!(r.get("code"), Some(&Value::Int32(11)), "{r:?}");
    drop(node);
}

/// The audit log (013 FR-016): a file created with permissions 0600 that records the startup,
/// the logins, the user commands without their passwords, the refused commands, the logouts and
/// the shutdown, as RefDb Enterprise 9.0.4 does with `--auditDestination file`.
#[test]
fn the_audit_log_records_logins_users_and_refusals_without_secrets() {
    let d = dir("audit");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let path = d.0.join("audit.json");
    let audit = [
        "--auditDestination",
        "file",
        "--auditFormat",
        "JSON",
        "--auditPath",
        path.to_str().unwrap(),
    ];
    let node = start_with(&data, &log, &audit);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut root = root_client(node.port, &ca, &data);
    let roles = Value::Array(vec![Value::Document(
        Document::new().with("role", "read").with("db", "test"),
    )]);
    let r = root.command(
        "test",
        Document::new()
            .with("createUser", "auditor")
            .with("pwd", "auditor-pw-77")
            .with("roles", roles),
    );
    assert!(ok(&r), "{r:?}");
    drop(root);
    let mut stranger = Client::new(node.port, &ca);
    assert!(!ok(&stranger.command("test", Document::new().with("find", "c"))));
    drop(stranger);
    drop(node);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = std::fs::read_to_string(&path).unwrap();
    let (user, password) = credentials(&data);
    assert!(!text.contains(&password) && !text.contains("auditor-pw-77"));
    let atypes: Vec<&str> = text
        .lines()
        .map(|l| l.strip_prefix(r#"{ "atype" : ""#).and_then(|r| r.split('"').next()).unwrap())
        .collect();
    assert_eq!(atypes.first(), Some(&"startup"), "{text}");
    assert_eq!(atypes.last(), Some(&"shutdown"), "{text}");
    let login = format!(
        r#""param" : {{ "user" : "{user}", "db" : "admin", "mechanism" : "SCRAM-SHA-256" }}, "result" : 0 }}"#
    );
    assert!(text.lines().any(|l| l.ends_with(&login)), "{text}");
    assert!(text.contains(r#"{ "atype" : "createUser""#), "{text}");
    assert!(text.contains(r#""param" : { "user" : "auditor", "db" : "test", "roles" : [ { "role" : "read", "db" : "test" } ] }"#));
    assert!(
        text.contains(r#""param" : { "command" : "find", "ns" : "test" }, "result" : 13 }"#),
        "{text}"
    );
    assert!(text.contains(r#"{ "atype" : "logout""#), "{text}");
}

/// Bad audit options stop the start with RefDb's message.
#[test]
fn bad_audit_options_stop_the_start() {
    let (code, err) = run_once(&["start", "--auditDestination", "file", "--auditFormat", "JSON"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("auditLog.path must be specified when auditLog.destination is to a file"),
        "{err}"
    );
}

/// A server of simple binds written from RFC 4511 for the test: `uid=<user>,ou=people` with
/// password `<user>-pw` binds, anything else gets `invalidCredentials`.
/// Group searches the fake LDAP server answered.
static GROUP_SEARCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// An LDAP server for the tests: `uid=<user>,ou=people` binds with `<user>-pw`, and a search
/// whose filter names `uid=jane,ou=people` finds the group `cn=readers,ou=groups`.
fn fake_ldap() -> u16 {
    use teseladb_auth::ldap_ber::{self, tag};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(n) = sock.read(&mut chunk) else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    while let Ok(Some((_, content, rest))) = ldap_ber::read(&buf) {
                        let used = buf.len() - rest.len();
                        let (id, op) = ldap_ber::expect(content, tag::INTEGER).unwrap();
                        let (t, body, _) = ldap_ber::read_whole(op).unwrap();
                        if t == 0x60 {
                            let parts = ldap_ber::elements(body).unwrap();
                            let dn = String::from_utf8_lossy(parts[1].1).into_owned();
                            let user =
                                dn.strip_prefix("uid=").and_then(|d| d.strip_suffix(",ou=people"));
                            let ok =
                                user.is_some_and(|u| parts[2].1 == format!("{u}-pw").as_bytes());
                            let mut r =
                                ldap_ber::element(tag::ENUMERATED, &[if ok { 0 } else { 49 }]);
                            ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
                            ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
                            let mut m = ldap_ber::element(tag::INTEGER, id);
                            m.extend_from_slice(&ldap_ber::element(0x61, &r));
                            let _ = sock.write_all(&ldap_ber::element(tag::SEQUENCE, &m));
                        }
                        if t == 0x63 {
                            GROUP_SEARCHES.fetch_add(1, Ordering::SeqCst);
                            let answer = |op: Vec<u8>| {
                                let mut m = ldap_ber::element(tag::INTEGER, id);
                                m.extend_from_slice(&op);
                                ldap_ber::element(tag::SEQUENCE, &m)
                            };
                            let jane = b"uid=jane,ou=people";
                            if body.windows(jane.len()).any(|w| w == jane) {
                                let mut e =
                                    ldap_ber::element(tag::OCTET_STRING, b"cn=readers,ou=groups");
                                ldap_ber::put(&mut e, tag::SEQUENCE, &[]);
                                let _ = sock.write_all(&answer(ldap_ber::element(0x64, &e)));
                            }
                            let mut r = ldap_ber::element(tag::ENUMERATED, &[0]);
                            ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
                            ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
                            let _ = sock.write_all(&answer(ldap_ber::element(0x65, &r)));
                        }
                        buf.drain(..used);
                    }
                }
            });
        }
    });
    port
}

/// PLAIN against LDAP end to end (013 FR-025): with `authenticationMechanisms` naming it and
/// `--ldapServers`, a user of `$external` logs in with the password the LDAP server takes, and
/// with a wrong one gets `Authentication failed.`.
#[test]
fn plain_logs_in_through_the_ldap_server() {
    let d = dir("ldap");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let server = format!("127.0.0.1:{}", fake_ldap());
    let args = [
        "--ldapServers",
        server.as_str(),
        "--ldapTransportSecurity",
        "none",
        "--ldapUserToDNMapping",
        "[{match: '(.+)', substitution: 'uid={0},ou=people'}]",
        "--setParameter",
        "authenticationMechanisms=PLAIN,SCRAM-SHA-256",
    ];
    let node = start_with(&data, &log, &args);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut root = root_client(node.port, &ca, &data);
    let roles = Value::Array(vec![Value::Document(
        Document::new().with("role", "read").with("db", "test"),
    )]);
    let r =
        root.command("$external", Document::new().with("createUser", "jane").with("roles", roles));
    assert!(ok(&r), "{r:?}");
    let plain = |password: &str| {
        let mut c = Client::new(node.port, &ca);
        let payload = format!("\0jane\0{password}").into_bytes();
        let start = Document::new()
            .with("saslStart", 1)
            .with("mechanism", "PLAIN")
            .with("payload", Value::Binary { subtype: 0, bytes: payload });
        c.command("$external", start)
    };
    let r = plain("jane-pw");
    assert!(ok(&r) && r.get("done") == Some(&Value::Boolean(true)), "{r:?}");
    let r = plain("wrong");
    assert_eq!(r.get("errmsg"), Some(&Value::from("Authentication failed.")), "{r:?}");
}

/// Authorization by LDAP groups end to end (013 FR-026): with `--ldapAuthzQueryTemplate` the
/// user needs no document in `$external`, its group is its role in `admin` with what it
/// inherits, and the groups are kept until `invalidateUserCache` or the interval ends.
#[test]
fn ldap_groups_are_the_roles_of_a_plain_login() {
    let d = dir("ldap-authz");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let server = format!("127.0.0.1:{}", fake_ldap());
    let args = [
        "--ldapServers",
        server.as_str(),
        "--ldapTransportSecurity",
        "none",
        "--ldapUserToDNMapping",
        "[{match: '(.+)', substitution: 'uid={0},ou=people'}]",
        "--ldapAuthzQueryTemplate",
        "ou=groups??one?(member={USER})",
        "--setParameter",
        "authenticationMechanisms=PLAIN,SCRAM-SHA-256",
        "--setParameter",
        "ldapUserCacheInvalidationInterval=600",
    ];
    let node = start_with(&data, &log, &args);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut root = root_client(node.port, &ca, &data);
    let inherits = Value::Array(vec![Value::Document(
        Document::new().with("role", "read").with("db", "test"),
    )]);
    let role = Document::new()
        .with("createRole", "cn=readers,ou=groups")
        .with("privileges", Value::Array(Vec::new()))
        .with("roles", inherits);
    let r = root.command("admin", role);
    assert!(ok(&r), "{r:?}");
    let login = || {
        let mut c = Client::new(node.port, &ca);
        let start = Document::new()
            .with("saslStart", 1)
            .with("mechanism", "PLAIN")
            .with("payload", Value::Binary { subtype: 0, bytes: b"\0jane\0jane-pw".to_vec() });
        let r = c.command("$external", start);
        assert!(ok(&r), "{r:?}");
        c
    };
    let before = GROUP_SEARCHES.load(Ordering::SeqCst);
    let mut jane = login();
    let r = jane.command("admin", Document::new().with("connectionStatus", 1));
    let Some(Value::Document(info)) = r.get("authInfo") else { panic!("{r:?}") };
    let role = |r: &str, d: &str| Value::Document(Document::new().with("role", r).with("db", d));
    assert_eq!(
        info.get("authenticatedUserRoles"),
        Some(&Value::Array(vec![role("cn=readers,ou=groups", "admin"), role("read", "test")]))
    );
    assert!(ok(&jane.command("test", Document::new().with("find", "c"))));
    // Kept: the next login does not search; after `invalidateUserCache` it does.
    login();
    assert_eq!(GROUP_SEARCHES.load(Ordering::SeqCst) - before, 1);
    assert!(ok(&root.command("admin", Document::new().with("invalidateUserCache", 1))));
    login();
    assert_eq!(GROUP_SEARCHES.load(Ordering::SeqCst) - before, 2);
    drop(node);
}

/// REFDB-AWS end to end (013 FR-025) up to the STS: on `$external` the node answers with its
/// nonce and the STS host of `awsSTSUrl`; an STS that does not answer fails the login with
/// `Authentication failed.`. `awsSTSUrl` must be HTTPS.
#[test]
fn refdb_aws_reaches_the_sts_of_aws_sts_url() {
    let d = dir("aws");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let sts = format!("awsSTSUrl=https://127.0.0.1:{dead}");
    let args = [
        "--setParameter",
        "authenticationMechanisms=REFDB-AWS,SCRAM-SHA-256",
        "--setParameter",
        sts.as_str(),
    ];
    let node = start_with(&data, &log, &args);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut c = Client::new(node.port, &ca);
    let first = Document::new()
        .with("r", Value::Binary { subtype: 0, bytes: vec![3; 32] })
        .with("p", 110)
        .to_bytes()
        .unwrap();
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", "REFDB-AWS")
        .with("payload", Value::Binary { subtype: 0, bytes: first });
    let r = c.command("$external", start);
    assert_eq!(r.get("done"), Some(&Value::Boolean(false)), "{r:?}");
    let Some(Value::Binary { bytes, .. }) = r.get("payload") else { panic!("{r:?}") };
    let reply = Document::from(RawDoc::from_bytes(bytes).unwrap());
    assert_eq!(reply.get("h"), Some(&Value::from("127.0.0.1")));
    let auth = "AWS4-HMAC-SHA256 Credential=AKID/20261030/us-east-1/sts/aws4_request, SignedHeaders=content-length;content-type;host;x-amz-date;x-refdb-gs2-cb-flag;x-refdb-server-nonce, Signature=00";
    let second = Document::new().with("a", auth).with("d", "20261030T000000Z").to_bytes().unwrap();
    let cont = Document::new()
        .with("saslContinue", 1)
        .with("conversationId", 1)
        .with("payload", Value::Binary { subtype: 0, bytes: second });
    let r = c.command("$external", cont);
    assert_eq!(r.get("errmsg"), Some(&Value::from("Authentication failed.")), "{r:?}");
    drop(node);
    let (code, err) = run_once(&["start", "--setParameter", "awsSTSUrl=http://sts.test"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("STS URL must start with https://"), "{err}");
}

/// REFDB-OIDC end to end (013 FR-025) up to the provider: the two-step answer names the
/// provider of `oidcIdentityProviders`; a token whose provider cannot be reached fails the login
/// with `Authentication failed.`. A parameter that does not read stops the start.
#[test]
fn refdb_oidc_reaches_the_provider_of_its_issuer() {
    let d = dir("oidc");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let issuer = format!("https://127.0.0.1:{dead}");
    let providers = format!(
        r#"oidcIdentityProviders=[{{"issuer": "{issuer}", "audience": "tdb", "authNamePrefix": "idp", "clientId": "cid", "authorizationClaim": "groups"}}]"#
    );
    let args = [
        "--setParameter",
        "authenticationMechanisms=REFDB-OIDC,SCRAM-SHA-256",
        "--setParameter",
        providers.as_str(),
    ];
    let node = start_with(&data, &log, &args);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let start = |payload: Document| {
        Document::new()
            .with("saslStart", 1)
            .with("mechanism", "REFDB-OIDC")
            .with("payload", Value::Binary { subtype: 0, bytes: payload.to_bytes().unwrap() })
    };
    let r =
        Client::new(node.port, &ca).command("$external", start(Document::new().with("n", "alice")));
    let Some(Value::Binary { bytes, .. }) = r.get("payload") else { panic!("{r:?}") };
    let reply = Document::from(RawDoc::from_bytes(bytes).unwrap());
    assert_eq!(reply.get("issuer"), Some(&Value::from(issuer.as_str())));
    let b64 = |s: &str| {
        teseladb_x509::pem::base64_encode(s.as_bytes())
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_")
    };
    let jwt = format!(
        "{}.{}.AAAA",
        b64(r#"{"alg":"RS256","kid":"k1"}"#),
        b64(&format!(r#"{{"iss":"{issuer}","aud":"tdb","sub":"a","exp":4000000000}}"#))
    );
    let r =
        Client::new(node.port, &ca).command("$external", start(Document::new().with("jwt", jwt)));
    assert_eq!(r.get("errmsg"), Some(&Value::from("Authentication failed.")), "{r:?}");
    drop(node);
    let (code, err) = run_once(&["start", "--setParameter", "oidcIdentityProviders=[1]"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("oidcIdentityProviders"), "{err}");
}

/// The LDAP options and `authenticationMechanisms` stop a start with a bad value.
#[test]
fn bad_ldap_options_stop_the_start() {
    for (v, msg) in [
        ("0", "ldapUserCacheInvalidationInterval must be between 1 and 86400 (24 hours)"),
        ("86401", "ldapUserCacheInvalidationInterval must be between 1 and 86400 (24 hours)"),
        ("x", "ldapUserCacheInvalidationInterval must be a numeric value, 'x' provided"),
    ] {
        let p = format!("ldapUserCacheInvalidationInterval={v}");
        let (code, err) = run_once(&["start", "--setParameter", &p]);
        assert_eq!(code, 2, "{err}");
        assert!(err.contains(msg), "{err}");
    }
    // A data directory of its own: these starts get as far as the PKI.
    let d = dir("bad-ldap");
    let data = d.0.join("data");
    let data = data.to_str().unwrap();
    let (code, err) = run_once(&["start", "--dbpath", data, "--ldapAuthzQueryTemplate", "dc=x"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("need --ldapServers"), "{err}");
    let (code, err) = run_once(&[
        "start",
        "--dbpath",
        data,
        "--ldapServers",
        "127.0.0.1:1",
        "--ldapAuthzQueryTemplate",
        "dc=x?a?wrong",
    ]);
    assert_ne!(code, 0, "{err}");
    assert!(err.contains("authz.queryTemplate"), "{err}");
    let (code, err) = run_once(&["start", "--setParameter", "authenticationMechanisms=FOO"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("Mechanism FOO is not supported"), "{err}");
    let (code, err) = run_once(&["start", "--ldapTimeoutMS", "soon"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("ldapTimeoutMS"), "{err}");
}

/// `teseladb admin cert rotate` (013 T018, FR-006): a new node key and certificate of the
/// cluster's CA, served at once on the clients' port without a restart.
#[test]
fn the_admin_command_rotates_the_node_certificate() {
    let d = dir("cert-rotate");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let node = start(&data, &log);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let admin = node.admin.clone();
    let first = served_leaf(node.port, &ca);
    let admin_run = |extra: &[&str]| {
        let args: Vec<String> = ["admin", "--node", &admin]
            .iter()
            .chain(extra)
            .chain(&["cert", "rotate"])
            .map(|s| s.to_string())
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &mut out, &mut err, &|| true);
        (code, out, err)
    };
    // Without credentials the port refuses the command (013 FR-017).
    let none = d.0.join("none");
    let (code, _, err) = admin_run(&["--dbpath", none.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert_eq!(
        String::from_utf8_lossy(&err),
        "teseladb admin: the administration port needs a client certificate over TLS\n"
    );
    assert_eq!(served_leaf(node.port, &ca), first);
    // On the node, the node's own certificate is a member of the cluster.
    let (code, out, err) = admin_run(&["--dbpath", data.to_str().unwrap()]);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(String::from_utf8(out).unwrap(), "certificates rotated\n");
    let renewed = disk_leaf(&data);
    assert_ne!(renewed, first);
    assert_eq!(served_leaf(node.port, &ca), renewed);
    let leaf = Certificate::from_der(&renewed).unwrap();
    assert!(leaf.verify_signed_by(&ca.public_key).is_ok());
    drop(node);
}

/// The administration port of a secure node (013 FR-017): TLS, `/healthz` for anyone, and the
/// rest for a client certificate whose user has the privileges of `clusterMonitor` (`/metrics`)
/// or `clusterAdmin` (the commands).
#[test]
fn the_administration_port_asks_for_tls_and_roles() {
    use teseladb_server::admin_http::{Method, request, request_with};
    use teseladb_x509::oid;
    let d = dir("admin-roles");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let node = start(&data, &log);
    let ca_file = data.join(CA_CERT);
    let ca =
        Certificate::from_pem_all(&std::fs::read_to_string(&ca_file).unwrap()).unwrap().remove(0);
    let admin = node.admin.clone();
    let mut root = root_client(node.port, &ca, &data);
    let roles = Value::Array(vec![Value::Document(
        Document::new().with("role", "clusterMonitor").with("db", "admin"),
    )]);
    let subject = "CN=ops,OU=operators,O=teseladb";
    let r =
        root.command("$external", Document::new().with("createUser", subject).with("roles", roles));
    assert!(ok(&r), "{r:?}");
    // Without TLS only `/healthz` answers.
    assert_eq!(request(&admin, Method::Get, "/healthz", b"").unwrap(), (200, Vec::new()));
    assert_eq!(request(&admin, Method::Get, "/metrics", b"").unwrap().0, 401);
    // The operator's certificate, of the cluster's CA but not of a node, in a PEM file.
    let name: [(&[u8], &str); 3] = [
        (oid::ORGANIZATION, "teseladb"),
        (oid::ORGANIZATIONAL_UNIT, "operators"),
        (oid::COMMON_NAME, "ops"),
    ];
    let id = client_identity(&data, &name);
    let pem = format!(
        "{}{}",
        id.chain[0].to_pem(),
        teseladb_x509::pem::encode("PRIVATE KEY", &id.key.to_pkcs8())
    );
    let pem_file = d.0.join("ops.pem");
    std::fs::write(&pem_file, pem).unwrap();
    let mut client = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca.clone()]));
    client.identity = Some(id);
    let (status, body) =
        request_with(&admin, Some(Arc::new(client)), Method::Get, "/metrics", b"").unwrap();
    assert_eq!(status, 200);
    assert!(String::from_utf8(body).unwrap().contains("teseladb_up 1"));
    // `clusterMonitor` is not enough for a command.
    let cli = |extra: &[&str]| {
        let args: Vec<String> = ["admin", "--node", &admin]
            .iter()
            .chain(extra)
            .chain(&["nodes"])
            .map(|s| s.to_string())
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &mut out, &mut err, &|| true);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    };
    let (pem_arg, ca_arg) = (pem_file.to_str().unwrap(), ca_file.to_str().unwrap());
    let (code, _, err) = cli(&["--tlsCertificateKeyFile", pem_arg, "--tlsCAFile", ca_arg]);
    assert_eq!(code, 1);
    assert_eq!(
        err,
        format!(
            "teseladb admin: $external.{subject} is not authorized: /admin/nodes needs the privileges of clusterAdmin\n"
        )
    );
    let r = root.command(
        "$external",
        Document::new().with("grantRolesToUser", subject).with(
            "roles",
            Value::Array(vec![Value::Document(
                Document::new().with("role", "clusterAdmin").with("db", "admin"),
            )]),
        ),
    );
    assert!(ok(&r), "{r:?}");
    let (code, out, err) = cli(&["--tlsCertificateKeyFile", pem_arg, "--tlsCAFile", ca_arg]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.is_empty());
    // A certificate file without its CA file, and files that do not read, stop the command.
    let (code, _, err) = cli(&["--tlsCertificateKeyFile", pem_arg]);
    assert_eq!(
        (code, err.as_str()),
        (1, "teseladb admin: --tlsCertificateKeyFile needs --tlsCAFile\n")
    );
    let missing = d.0.join("missing.pem");
    let missing = missing.to_str().unwrap();
    for args in [
        ["--tlsCertificateKeyFile", missing, "--tlsCAFile", ca_arg],
        ["--tlsCertificateKeyFile", pem_arg, "--tlsCAFile", missing],
        ["--tlsCertificateKeyFile", ca_arg, "--tlsCAFile", ca_arg],
        ["--tlsCertificateKeyFile", pem_arg, "--tlsCAFile", pem_arg.trim_end_matches(".pem")],
    ] {
        let (code, _, err) = cli(&args);
        assert_eq!(code, 1, "{args:?}: {err}");
    }
    // A data directory whose PKI does not load.
    let broken = d.0.join("broken");
    std::fs::create_dir_all(broken.join("pki")).unwrap();
    std::fs::write(broken.join(CA_CERT), "garbage").unwrap();
    let (code, _, err) = cli(&["--dbpath", broken.to_str().unwrap()]);
    assert_eq!(code, 1, "{err}");
    drop(node);
}

/// The lines of the secret files of the data directory that could show up in a log: the master
/// key in base64 and in hexadecimal, the body of each private key and the root password.
fn data_secrets(data: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for file in [MASTER_KEY, CA_KEY, NODE_KEY] {
        let text = std::fs::read_to_string(data.join(file)).unwrap();
        for line in text.lines().map(str::trim).filter(|l| l.len() >= 12 && !l.starts_with("-----"))
        {
            out.push((file.to_owned(), line.to_owned()));
        }
    }
    let master = std::fs::read_to_string(data.join(MASTER_KEY)).unwrap();
    let raw = teseladb_x509::pem::base64_decode(master.trim().as_bytes()).unwrap();
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    out.push((format!("{MASTER_KEY} (hex)"), hex.to_uppercase()));
    out.push((format!("{MASTER_KEY} (hex)"), hex));
    out.push(("root password".to_owned(), credentials(data).1));
    out
}

/// No password or key of the node in its log, its diagnostics or its metrics (013 FR-023, T030):
/// the node's half of the battery's scan, on one node with logins, a refused login, a user with a
/// password and an operator reading `/metrics`.
#[test]
fn secrets_stay_out_of_the_log_the_diagnostics_and_the_metrics() {
    use teseladb_server::admin_http::{Method, request_with};
    use teseladb_x509::oid;
    const PASSWORD: &str = "scanned-pw-0123456789";
    let d = dir("secrets");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let node = start(&data, &log);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut root = root_client(node.port, &ca, &data);
    let read = Value::Array(vec![Value::Document(
        Document::new().with("role", "readAnyDatabase").with("db", "admin"),
    )]);
    let r = root.command(
        "admin",
        Document::new().with("createUser", "scanned").with("pwd", PASSWORD).with("roles", read),
    );
    assert!(ok(&r), "{r:?}");
    let mut c = Client::new(node.port, &ca);
    assert!(!ok(&login(&mut c, "scanned", "wrong-pw-9876543210")));
    let mut c = Client::new(node.port, &ca);
    assert!(ok(&login(&mut c, "scanned", PASSWORD)));
    let r = root.command(
        "admin",
        Document::new().with("updateUser", "scanned").with("pwd", "new-pw-0123456789"),
    );
    assert!(ok(&r), "{r:?}");
    // What the node reports about itself.
    let mut seen = String::new();
    for cmd in ["serverStatus", "getCmdLineOpts", "hostInfo", "currentOp", "connectionStatus"] {
        let r = root.command("admin", Document::new().with(cmd, 1));
        assert!(ok(&r), "{cmd}: {r:?}");
        seen.push_str(&format!("{r:?}\n"));
    }
    let r = root.command("admin", Document::new().with("getLog", "global"));
    assert!(ok(&r), "{r:?}");
    seen.push_str(&format!("{r:?}\n"));
    // The metrics, read by an operator with `clusterMonitor`.
    let admin = node.admin.clone();
    let roles = Value::Array(vec![Value::Document(
        Document::new().with("role", "clusterMonitor").with("db", "admin"),
    )]);
    let subject = "CN=ops,OU=operators,O=teseladb";
    let r =
        root.command("$external", Document::new().with("createUser", subject).with("roles", roles));
    assert!(ok(&r), "{r:?}");
    let name: [(&[u8], &str); 3] = [
        (oid::ORGANIZATION, "teseladb"),
        (oid::ORGANIZATIONAL_UNIT, "operators"),
        (oid::COMMON_NAME, "ops"),
    ];
    let mut client = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca.clone()]));
    client.identity = Some(client_identity(&data, &name));
    let (status, body) =
        request_with(&admin, Some(Arc::new(client)), Method::Get, "/metrics", b"").unwrap();
    assert_eq!(status, 200);
    seen.push_str(&String::from_utf8(body).unwrap());
    drop((c, root, node));
    seen.push_str(&std::fs::read_to_string(&log).unwrap());
    let mut secrets = data_secrets(&data);
    for p in [PASSWORD, "wrong-pw-9876543210", "new-pw-0123456789"] {
        secrets.push(("a user's password".to_owned(), p.to_owned()));
    }
    for (name, value) in &secrets {
        // The name of what leaked, never its value.
        assert!(
            !seen.contains(value.as_str()),
            "{name} is in the log, the diagnostics or the metrics"
        );
    }
}

fn start(data: &Path, log: &Path) -> Started {
    start_with(data, log, &[])
}

fn credentials(data: &Path) -> (String, String) {
    let text = std::fs::read_to_string(data.join(ROOT_CREDENTIALS)).unwrap();
    let get =
        |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap().to_owned();
    (get("user"), get("password"))
}

/// A client logged in as `root`, once core 0 created it.
fn root_client(port: u16, ca: &Certificate, data: &Path) -> Client {
    let (user, password) = credentials(data);
    let until = Instant::now() + READY_WAIT;
    loop {
        let mut c = Client::new(port, ca);
        if ok(&login(&mut c, &user, &password)) {
            return c;
        }
        assert!(Instant::now() < until, "root never came");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A client certificate of the node's CA for `subject`, as `teseladb cert client` makes it.
fn client_identity(data: &Path, subject: &[(&[u8], &str)]) -> teseladb_tls::CertifiedKey {
    use teseladb_x509::generate::{self, CertificateParams};
    use teseladb_x509::{Name, PrivateKey};
    let pki = teseladb_server::pki::load(data).unwrap();
    let key = PrivateKey::generate_ec(teseladb_crypto::ec::NamedCurve::P256, &[7; 40]).unwrap();
    let now = now_ms() / 1000;
    let p = CertificateParams::node([5; 16], Name::build(subject), now - 60, now + 86_400);
    let cert = generate::issue(&p, &key.public_key(), &pki.ca, &pki.ca_key).unwrap();
    teseladb_tls::CertifiedKey { chain: vec![cert], key }
}
