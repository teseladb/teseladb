//! The master key from a KMIP server (013 FR-028), against a KMIP server of the test that speaks
//! TLS with client certificates and keeps its keys and their states: the first start creates and
//! activates a key and seals the node's own key with it, the next ones open it, a rotation seals it
//! under a new key, and a key that is not active, unknown or out of reach stops the start.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use teseladb_auth::kmip::{ACTIVE, Item, Value, message_len, operation, tag};
use teseladb_server::kmip::{self, KEYSTORE, KmipOpts};
use teseladb_server::pki::{self, PkiParams};
use teseladb_server::tls_stream::TlsStream;
use teseladb_tls::{CertifiedKey, ClientAuth, ServerConfig, Trust};
use teseladb_x509::ext::GeneralName;

/// State: Pre-Active.
const PRE_ACTIVE: u32 = 1;
/// State: Compromised.
const COMPROMISED: u32 = 4;

#[derive(Default)]
struct Keys {
    next: u32,
    keys: BTreeMap<String, ([u8; 32], u32)>,
    /// Operations and protocol minor versions, in order.
    seen: Vec<(u32, i32)>,
    /// Activations fail.
    refuse_activate: bool,
}

/// A key identifier whose Get makes the server close the connection.
const HANG_UP: &str = "hang-up";

type Shared = Arc<Mutex<Keys>>;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-kmip-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn text(t: u32, s: &str) -> Item {
    Item::new(t, Value::TextString(s.to_owned()))
}

fn enumeration(t: u32, v: u32) -> Item {
    Item::new(t, Value::Enumeration(v))
}

fn structure(t: u32, items: Vec<Item>) -> Item {
    Item::new(t, Value::Structure(items))
}

fn id_of(payload: &Item) -> String {
    match &payload.child(tag::UNIQUE_IDENTIFIER).unwrap().value {
        Value::TextString(s) => s.clone(),
        other => panic!("{other:?}"),
    }
}

/// The answer of the test's server to one request.
fn answer(keys: &Shared, request: &[u8]) -> Vec<u8> {
    let (msg, _) = Item::decode(request).unwrap();
    let minor = match msg
        .child(tag::REQUEST_HEADER)
        .and_then(|h| h.child(tag::PROTOCOL_VERSION))
        .and_then(|v| v.child(tag::PROTOCOL_VERSION_MINOR))
        .map(|m| &m.value)
    {
        Some(Value::Integer(m)) => *m,
        other => panic!("{other:?}"),
    };
    let item = msg.child(tag::BATCH_ITEM).unwrap();
    let op = match item.child(tag::OPERATION).unwrap().value {
        Value::Enumeration(o) => o,
        ref other => panic!("{other:?}"),
    };
    let payload = item.child(tag::REQUEST_PAYLOAD).unwrap();
    let mut k = keys.lock().unwrap();
    k.seen.push((op, minor));
    let failed = |message: String| {
        vec![
            enumeration(tag::RESULT_STATUS, 1),
            enumeration(tag::RESULT_REASON, 1),
            text(tag::RESULT_MESSAGE, &message),
        ]
    };
    let ok = |items: Vec<Item>| {
        vec![enumeration(tag::RESULT_STATUS, 0), structure(tag::RESPONSE_PAYLOAD, items)]
    };
    let body = match op {
        operation::CREATE => {
            k.next += 1;
            let id = k.next.to_string();
            let bytes = [u8::try_from(k.next).unwrap(); 32];
            k.keys.insert(id.clone(), (bytes, PRE_ACTIVE));
            ok(vec![enumeration(tag::OBJECT_TYPE, 2), text(tag::UNIQUE_IDENTIFIER, &id)])
        }
        _ => {
            let id = id_of(payload);
            let refuse = k.refuse_activate;
            match (op, k.keys.get_mut(&id)) {
                (_, None) => failed(format!("Could not locate object: {id}")),
                (operation::ACTIVATE, Some(_)) if refuse => {
                    failed(format!("Not allowed to activate {id}"))
                }
                (operation::ACTIVATE, Some(entry)) => {
                    entry.1 = ACTIVE;
                    ok(vec![text(tag::UNIQUE_IDENTIFIER, &id)])
                }
                // Encrypt (the audit log's key, 013 T042): the data reversed, no IV.
                (operation::ENCRYPT, Some(_)) => {
                    let data = match payload.child(tag::DATA).map(|d| &d.value) {
                        Some(Value::ByteString(d)) => d.iter().rev().copied().collect(),
                        _ => Vec::new(),
                    };
                    ok(vec![
                        text(tag::UNIQUE_IDENTIFIER, &id),
                        Item::new(tag::DATA, Value::ByteString(data)),
                    ])
                }
                (operation::GET_ATTRIBUTES, Some(entry)) => ok(vec![
                    text(tag::UNIQUE_IDENTIFIER, &id),
                    structure(
                        tag::ATTRIBUTE,
                        vec![
                            text(tag::ATTRIBUTE_NAME, "State"),
                            enumeration(tag::ATTRIBUTE_VALUE, entry.1),
                        ],
                    ),
                ]),
                (_, Some(entry)) => {
                    let block = structure(
                        tag::KEY_BLOCK,
                        vec![
                            enumeration(tag::KEY_FORMAT_TYPE, 1),
                            structure(
                                tag::KEY_VALUE,
                                vec![Item::new(
                                    tag::KEY_MATERIAL,
                                    Value::ByteString(entry.0.to_vec()),
                                )],
                            ),
                        ],
                    );
                    ok(vec![
                        enumeration(tag::OBJECT_TYPE, 2),
                        text(tag::UNIQUE_IDENTIFIER, &id),
                        structure(tag::SYMMETRIC_KEY, vec![block]),
                    ])
                }
            }
        }
    };
    let mut items = vec![enumeration(tag::OPERATION, op)];
    items.extend(body);
    let header = structure(tag::RESPONSE_HEADER, Vec::new());
    structure(tag::RESPONSE_MESSAGE, vec![header, structure(tag::BATCH_ITEM, items)]).encode()
}

/// A KMIP server on 127.0.0.1 with the certificate of `pki`, asking for client certificates of
/// its CA; its port.
fn server(pki: &pki::Pki, keys: Shared) -> u16 {
    let identity = CertifiedKey { chain: pki.node_chain.clone(), key: pki.node_key.clone() };
    let mut config = ServerConfig::new(identity);
    config.client_auth = ClientAuth::Required;
    config.trust = Trust::anchors(vec![pki.ca.clone()]);
    // A server with TLS 1.2 only, like many KMIP servers: the node's client speaks it (013 T041).
    config.min_version = teseladb_tls::Version::Tls12;
    config.max_version = teseladb_tls::Version::Tls12;
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let (config, keys) = (config.clone(), keys.clone());
            std::thread::spawn(move || {
                let Ok(mut s) = TlsStream::server(sock, config, &[]) else { return };
                let mut buf = Vec::new();
                while let Ok(data) = s.recv() {
                    if data.is_empty() {
                        return;
                    }
                    buf.extend_from_slice(&data);
                    while let Some(len) = message_len(&buf).filter(|l| buf.len() >= *l) {
                        if buf.windows(HANG_UP.len()).any(|w| w == HANG_UP.as_bytes()) {
                            return;
                        }
                        let reply = answer(&keys, &buf[..len]);
                        buf.drain(..len);
                        // In two records: the client reads a message in pieces.
                        let (head, rest) = reply.split_at(5);
                        if s.send(head).is_err() || s.send(rest).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    port
}

struct Fixture {
    base: PathBuf,
    data: PathBuf,
    keys: Shared,
    opts: KmipOpts,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// A KMIP server and the options that reach it with a client certificate of its CA.
fn fixture(name: &str) -> Fixture {
    let base = dir(name);
    let mut random = |b: &mut [u8]| teseladb_sys::fill_random(b);
    let names = vec![GeneralName::Ip(vec![127, 0, 0, 1])];
    let server_pki = pki::load_or_create(
        &base.join("server"),
        &PkiParams { now: now(), node_names: names.clone() },
        &mut random,
    )
    .unwrap();
    let keys = Shared::default();
    let port = server(&server_pki, keys.clone());
    // The node's certificate, of the server's CA.
    let node_key =
        teseladb_x509::PrivateKey::generate_ec(teseladb_crypto::ec::NamedCurve::P256, &[9; 40])
            .unwrap();
    let cert = pki::issue_node(
        &server_pki.ca,
        &server_pki.ca_key,
        &node_key.public_key(),
        names,
        now(),
        &mut random,
    )
    .unwrap();
    let pem = format!(
        "{}{}",
        cert.to_pem(),
        teseladb_x509::pem::encode("PRIVATE KEY", &node_key.to_pkcs8())
    );
    let client = base.join("client.pem");
    std::fs::write(&client, pem).unwrap();
    let ca = base.join("ca.pem");
    std::fs::write(&ca, server_pki.ca.to_pem()).unwrap();
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let opts = KmipOpts {
        servers: vec!["127.0.0.1".into()],
        port,
        ca_file: Some(ca),
        client_certificate_file: Some(client),
        ..KmipOpts::default()
    };
    Fixture { base, data, keys, opts }
}

fn random(b: &mut [u8]) -> std::io::Result<()> {
    teseladb_sys::fill_random(b)
}

fn ops(f: &Fixture) -> Vec<u32> {
    let mut k = f.keys.lock().unwrap();
    std::mem::take(&mut k.seen).into_iter().map(|(op, _)| op).collect()
}

fn set_state(f: &Fixture, id: &str, state: u32) {
    f.keys.lock().unwrap().keys.get_mut(id).unwrap().1 = state;
}

#[test]
fn the_first_start_creates_the_key_and_the_next_ones_open_it() {
    let f = fixture("first");
    let first = kmip::master_key(&f.opts, &f.data, &mut random).unwrap();
    assert_eq!(first.id, "1");
    assert!(!first.rotated);
    assert_eq!(first.log, ["Created KMIP key keyId=1", "Activated KMIP key uid=1"]);
    assert_eq!(format!("{first:?}"), r#"MasterKey { id: "1", rotated: false }"#);
    use operation::{ACTIVATE, CREATE, GET, GET_ATTRIBUTES};
    assert_eq!(ops(&f), [CREATE, ACTIVATE, GET_ATTRIBUTES, GET]);
    let path = f.data.join(KEYSTORE);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("key=1\nsealed="), "{text}");
    // The node's own key is not the KMIP key and is not in the keystore in clear.
    assert_ne!(first.key, [1; 32]);
    let next = kmip::master_key(&f.opts, &f.data, &mut random).unwrap();
    assert_eq!((next.key, next.id.as_str(), next.log.len()), (first.key, "1", 0));
    assert_eq!(ops(&f), [GET_ATTRIBUTES, GET]);
    // KMIP 1.2 by default, 1.0 with the legacy protocol.
    let legacy = KmipOpts { legacy: true, ..f.opts.clone() };
    kmip::master_key(&legacy, &f.data, &mut random).unwrap();
    assert!(f.keys.lock().unwrap().seen.iter().all(|(_, minor)| *minor == 0));
    ops(&f);
    // In FIPS mode the TLS to the server keeps AES-GCM and the NIST curves (013 FR-029).
    let fips = KmipOpts { fips: true, ..f.opts.clone() };
    assert_eq!(kmip::master_key(&fips, &f.data, &mut random).unwrap().key, first.key);
    // Another identifier than the keystore's is refused.
    let other = KmipOpts { key_identifier: Some("7".into()), ..f.opts.clone() };
    let e = kmip::master_key(&other, &f.data, &mut random).unwrap_err();
    assert!(e.contains("encrypted with KMIP key '1'"), "{e}");
    // A key that is no longer active stops the start; without --kmipActivateKeys it is used.
    set_state(&f, "1", COMPROMISED);
    let e = kmip::master_key(&f.opts, &f.data, &mut random).unwrap_err();
    assert_eq!(e, "KMIP Key used for ESE is not in active state key_uid=1");
    let lax = KmipOpts { activate_keys: false, ..f.opts.clone() };
    assert_eq!(kmip::master_key(&lax, &f.data, &mut random).unwrap().key, first.key);
    // A damaged keystore, or one another key sealed.
    for damaged in ["key=1\nsealed=@@\n", "key=\nsealed=AAAA\n", "key=1\nsealed=AAAA\nmore\n"] {
        std::fs::write(&path, damaged).unwrap();
        let e = kmip::master_key(&lax, &f.data, &mut random).unwrap_err();
        assert_eq!(e, "kmip.keystore: damaged", "{damaged}");
    }
    std::fs::write(&path, format!("key=1\nsealed={}\n", "A".repeat(80))).unwrap();
    let e = kmip::master_key(&lax, &f.data, &mut random).unwrap_err();
    assert!(e.contains("does not open it"), "{e}");
}

#[test]
fn a_rotation_seals_the_same_key_under_a_new_one() {
    let f = fixture("rotate");
    let first = kmip::master_key(&f.opts, &f.data, &mut random).unwrap();
    ops(&f);
    let rotate = KmipOpts { rotate: true, ..f.opts.clone() };
    let r = kmip::master_key(&rotate, &f.data, &mut random).unwrap();
    assert!(r.rotated);
    assert_eq!((r.key, r.id.as_str()), (first.key, "2"));
    assert_eq!(
        r.log.last().map(String::as_str),
        Some("Rotated master encryption key oldMasterKeyId=1 newMasterKeyId=2")
    );
    let after = kmip::master_key(&f.opts, &f.data, &mut random).unwrap();
    assert_eq!((after.key, after.id.as_str()), (first.key, "2"));
    // The old key is not needed any more.
    set_state(&f, "1", COMPROMISED);
    assert!(kmip::master_key(&f.opts, &f.data, &mut random).is_ok());
}

#[test]
fn a_given_key_an_unknown_key_and_a_server_out_of_reach() {
    let f = fixture("given");
    // A key made beforehand: it has to be active already.
    let mut k = f.keys.lock().unwrap();
    k.keys.insert("pre".into(), ([5; 32], PRE_ACTIVE));
    drop(k);
    let given = KmipOpts { key_identifier: Some("pre".into()), ..f.opts.clone() };
    let e = kmip::master_key(&given, &f.data, &mut random).unwrap_err();
    assert_eq!(e, "KMIP Key used for ESE is not in active state key_uid=pre");
    set_state(&f, "pre", ACTIVE);
    let m = kmip::master_key(&given, &f.data, &mut random).unwrap();
    assert_eq!((m.id.as_str(), m.log.len()), ("pre", 0));
    // An identifier the server does not know: the reference server's message.
    let unknown =
        KmipOpts { key_identifier: Some("99".into()), activate_keys: false, ..f.opts.clone() };
    let fresh = f.base.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let e = kmip::master_key(&unknown, &fresh, &mut random).unwrap_err();
    assert_eq!(e, "KMIP get key '99' failed, code: 1 error: Could not locate object: 99");
    let unknown_state = KmipOpts { activate_keys: true, ..unknown.clone() };
    let e = kmip::master_key(&unknown_state, &fresh, &mut random).unwrap_err();
    assert!(e.starts_with("KMIP get attributes of key '99' failed"), "{e}");
    // Without --kmipActivateKeys a new key is not activated.
    let lax = KmipOpts { activate_keys: false, ..f.opts.clone() };
    let m = kmip::master_key(&lax, &fresh, &mut random).unwrap();
    assert_eq!(m.log, [format!("Created KMIP key keyId={}", m.id)]);
    // A server that does not listen, a name that does not resolve, and no server at all.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let gone = KmipOpts { port: dead, connect_retries: 1, ..f.opts.clone() };
    let e = kmip::master_key(&gone, &fresh, &mut random).unwrap_err();
    assert_eq!(e, format!("Could not connect to KMIP server 127.0.0.1:{dead}"));
    let nowhere = KmipOpts { servers: vec!["no-such-host.invalid".into()], ..f.opts.clone() };
    assert!(kmip::master_key(&nowhere, &fresh, &mut random).unwrap_err().contains("resolve"));
    let none = KmipOpts { servers: Vec::new(), ..f.opts.clone() };
    assert_eq!(kmip::master_key(&none, &fresh, &mut random).unwrap_err(), "no --kmipServerName");
    // The second server answers when the first does not.
    let both = KmipOpts {
        servers: vec!["no-such-host.invalid".into(), "127.0.0.1".into()],
        ..lax.clone()
    };
    assert!(kmip::master_key(&both, &fresh, &mut random).is_ok());
    // Certificate files that do not read, and a server whose certificate is not trusted.
    let missing = f.base.join("missing.pem");
    for o in [
        KmipOpts { ca_file: Some(missing.clone()), ..f.opts.clone() },
        KmipOpts { client_certificate_file: Some(missing.clone()), ..f.opts.clone() },
        KmipOpts { ca_file: f.opts.client_certificate_file.clone(), ..f.opts.clone() },
    ] {
        assert!(kmip::master_key(&o, &fresh, &mut random).is_err());
    }
    let garbage = f.base.join("garbage.pem");
    std::fs::write(&garbage, "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
        .unwrap();
    for o in [
        KmipOpts { ca_file: Some(garbage.clone()), ..f.opts.clone() },
        KmipOpts { client_certificate_file: Some(garbage.clone()), ..f.opts.clone() },
    ] {
        assert!(kmip::master_key(&o, &fresh, &mut random).unwrap_err().contains("--kmip"));
    }
    // A random source that fails, at once or for the node's key.
    let mut broken = |_: &mut [u8]| Err(std::io::Error::other("no entropy"));
    assert!(kmip::master_key(&f.opts, &fresh, &mut broken).unwrap_err().contains("no entropy"));
    let empty = f.base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let mut calls = 0;
    let mut second = |b: &mut [u8]| {
        calls += 1;
        if calls > 1 { Err(std::io::Error::other("no entropy")) } else { random(b) }
    };
    assert!(kmip::master_key(&lax, &empty, &mut second).unwrap_err().contains("no entropy"));
    // No client certificate (the server asks for one), a password, a certificate without its key.
    let anonymous = KmipOpts { client_certificate_file: None, ..lax.clone() };
    assert!(kmip::master_key(&anonymous, &empty, &mut random).is_err());
    let with_password = KmipOpts { client_certificate_password: Some("pw".into()), ..lax.clone() };
    let _ = kmip::master_key(&with_password, &empty, &mut random);
    let no_key = KmipOpts { client_certificate_file: f.opts.ca_file.clone(), ..lax.clone() };
    assert!(
        kmip::master_key(&no_key, &empty, &mut random)
            .unwrap_err()
            .contains("--kmipClientCertificateFile")
    );
    // A server that closes the connection, and one that refuses the activation.
    let hang = KmipOpts { key_identifier: Some(HANG_UP.into()), ..lax.clone() };
    let e = kmip::master_key(&hang, &f.base.join("hang"), &mut random).unwrap_err();
    assert!(e.contains("closed the connection"), "{e}");
    f.keys.lock().unwrap().refuse_activate = true;
    let e = kmip::master_key(&f.opts, &f.base.join("hang"), &mut random).unwrap_err();
    assert!(e.starts_with("KMIP activate key"), "{e}");
    f.keys.lock().unwrap().refuse_activate = false;
    // A data directory that does not exist, and a keystore that is a directory.
    let absent = f.base.join("absent");
    assert!(
        kmip::master_key(&lax, &absent, &mut random).unwrap_err().starts_with("kmip.keystore: ")
    );
    let odd = f.base.join("odd");
    std::fs::create_dir_all(odd.join(KEYSTORE)).unwrap();
    assert!(kmip::master_key(&lax, &odd, &mut random).unwrap_err().starts_with("kmip.keystore: "));
}

#[test]
fn a_key_that_stops_being_active_is_noticed() {
    let f = fixture("poll");
    let m = kmip::master_key(&f.opts, &f.data, &mut random).unwrap();
    let (stop, lost) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let every = Duration::from_millis(50);
    ops(&f);
    let poller = kmip::poll(f.opts.clone(), m.id.clone(), every, stop.clone(), lost.clone());
    // Two checks of an active key, then it is revoked.
    let until = Instant::now() + Duration::from_secs(10);
    let mut checks = 0;
    while checks < 2 {
        assert!(Instant::now() < until, "the poller never asked");
        checks += ops(&f).iter().filter(|o| **o == operation::GET_ATTRIBUTES).count();
        std::thread::sleep(every);
    }
    assert!(!lost.load(std::sync::atomic::Ordering::Relaxed));
    set_state(&f, &m.id, COMPROMISED);
    let until = Instant::now() + Duration::from_secs(10);
    while !lost.load(std::sync::atomic::Ordering::Relaxed) {
        assert!(Instant::now() < until, "the lost key was not noticed");
        std::thread::sleep(every);
    }
    poller.join().unwrap();
    assert_eq!(
        kmip::lost_message("1"),
        "KMIP Key used for ESE is not in active state. Shutting down server. key_uid=1"
    );
    // A server out of reach, or one that does not know the key, is asked again; a stop ends the
    // poller, also in the middle of a wait.
    let lost = Arc::new(AtomicBool::new(false));
    let gone = KmipOpts { servers: vec!["no-such-host.invalid".into()], ..f.opts.clone() };
    for (o, id) in [(gone, m.id.clone()), (f.opts.clone(), "unknown".to_owned())] {
        let stop = Arc::new(AtomicBool::new(false));
        let poller = kmip::poll(o, id, every, stop.clone(), lost.clone());
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        poller.join().unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let poller = kmip::poll(
        f.opts.clone(),
        m.id.clone(),
        Duration::from_secs(60),
        stop.clone(),
        lost.clone(),
    );
    std::thread::sleep(Duration::from_millis(150));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    poller.join().unwrap();
    assert!(!lost.load(std::sync::atomic::Ordering::Relaxed));
}

/// `teseladb start` with `args` in a thread; its exit code and log once it ends.
fn start(f: &Fixture, log: &std::path::Path, extra: &[&str]) -> std::thread::JoinHandle<i32> {
    let port = f.opts.port.to_string();
    let (ca, client) = (
        f.opts.ca_file.clone().unwrap().display().to_string(),
        f.opts.client_certificate_file.clone().unwrap().display().to_string(),
    );
    let mut args: Vec<String> = [
        "start",
        "--insecure",
        "--cores",
        "1",
        "--port",
        "0",
        "--peer-listen",
        "127.0.0.1:0",
        // A node of this machine may hold the default administration (27200) and HTTP (27080)
        // ports: this test needs neither on a known port, nor the REST API.
        "--admin-listen",
        "127.0.0.1:0",
        "--http-port",
        "0",
        "--enableEncryption",
        "--kmipServerName",
        "127.0.0.1",
        "--kmipPort",
        &port,
        "--kmipServerCAFile",
        &ca,
        "--kmipClientCertificateFile",
        &client,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(["--dbpath".into(), f.data.display().to_string()]);
    args.extend(["--logpath".into(), log.display().to_string()]);
    args.extend(extra.iter().map(|s| s.to_string()));
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = teseladb_server::run(&args, &mut out, &mut err, &|| {
            stop.load(std::sync::atomic::Ordering::Relaxed)
        });
        assert!(err.is_empty() || code != 0, "{}", String::from_utf8_lossy(&err));
        code
    })
}

fn wait_for(log: &std::path::Path, what: &str) -> String {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(what) {
            return text;
        }
        assert!(Instant::now() < until, "no {what:?} in {text}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A node started with KMIP: its files are encrypted under its own key, a rotation stops the
/// start with 0, and a key that stops being active stops the node with 1.
#[test]
fn a_node_starts_rotates_and_stops_with_its_kmip_key() {
    let f = fixture("node");
    let log = f.base.join("node.log");
    let node = start(&f, &log, &["--kmipKeyStatePollingSeconds", "1"]);
    let text = wait_for(&log, "clients' port addr=");
    assert!(text.contains("Created KMIP key keyId=1"), "{text}");
    assert!(text.contains("Activated KMIP key uid=1"), "{text}");
    assert!(f.data.join(KEYSTORE).exists());
    // The key is revoked: the node notices at its next check and stops.
    set_state(&f, "1", COMPROMISED);
    assert_eq!(node.join().unwrap(), 1);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains(&kmip::lost_message("1")), "{text}");
    // A rotation (with the old key active again) starts no node and ends with 0.
    set_state(&f, "1", ACTIVE);
    let rotation = start(&f, &log, &["--kmipRotateMasterKey"]);
    assert_eq!(rotation.join().unwrap(), 0);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("Rotated master encryption key oldMasterKeyId=1 newMasterKeyId=2"));
    assert!(std::fs::read_to_string(f.data.join(KEYSTORE)).unwrap().starts_with("key=2\n"));
    // A server out of reach stops the start.
    let gone = Fixture {
        opts: KmipOpts { port: 1, ..f.opts.clone() },
        base: f.base.join("gone"),
        data: f.data.clone(),
        keys: f.keys.clone(),
    };
    assert_eq!(start(&gone, &log, &[]).join().unwrap(), 1);
}

/// The audit log's key wrapped by the KMIP server (013 T042): the header names the KMIP key and
/// carries what Encrypt gave; an unknown key stops the start with the server's message.
#[test]
fn an_audit_log_key_wrapped_by_kmip() {
    use teseladb_server::audit_log::{AuditLog, AuditOpts};
    let f = fixture("audit");
    let mut random = |b: &mut [u8]| teseladb_sys::fill_random(b);
    let key = kmip::master_key(&f.opts, &f.data, &mut random).unwrap();
    assert!(!key.key.is_empty());
    let path = f.data.join("audit.json");
    let opts = |uid: &str| AuditOpts {
        destination: Some("file".into()),
        format: Some("JSON".into()),
        path: Some(path.to_str().unwrap().to_owned()),
        encryption_key_uid: Some(uid.to_owned()),
        kmip: Some(f.opts.clone()),
        ..AuditOpts::default()
    };
    let log = AuditLog::open(&opts("1")).unwrap().unwrap();
    log.system(teseladb_auth::audit::AuditType::Startup, teseladb_bson::Document::new());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(
            r#""keyStoreIdentifier":{"provider":"kmip","uid":"1","kmipServerName":["127.0.0.1"]"#
        ),
        "{text}"
    );
    assert!(text.lines().nth(1).is_some_and(|l| l.contains(r#""log":""#)), "{text}");
    let e = AuditLog::open(&opts("99")).unwrap_err();
    assert!(
        e.contains("KMIP encrypt with key '99' failed")
            && e.contains("Could not locate object: 99"),
        "{e}"
    );
}
