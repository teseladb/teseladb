//! Protocol violations each side must reject, driven with crafted messages
//! straight into the handshake state machines and with records protected by the peer's real
//! keys.

use std::sync::Arc;

use teseladb_crypto::ec::NamedCurve;
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Name, PrivateKey, oid};

use crate::codec::{put_vec8, put_vec16};
use crate::config::{CertifiedKey, ClientAuth, ClientConfig, ServerConfig, Trust};
use crate::conn::{Connection, State};
use crate::keyschedule::Secret;
use crate::kx::KEY_RANDOM_LEN;
use crate::msgs::{self, ClientHello, Extensions, ServerHello, ext, hs};
use crate::record::ContentType;
use crate::suites::{CipherSuite, NamedGroup};
use crate::{Alert, Error};

const NOW_MS: i64 = 1_800_000_000_000;
const HOST: &str = "neg.test";

fn configs() -> (Arc<ClientConfig>, Arc<ServerConfig>) {
    let now = NOW_MS / 1000;
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).expect("generate_ec succeeds");
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        now - 60,
        now + 86_400,
    );
    p.ca = Some(None);
    let ca = generate::self_signed(&p, &ca_key).expect("self_signed succeeds");
    let key = PrivateKey::ed25519(&[2; 32]);
    let mut lp = CertificateParams::node(
        [2; 16],
        Name::build(&[(oid::COMMON_NAME, HOST)]),
        now - 60,
        now + 86_400,
    );
    lp.alt_names = vec![GeneralName::Dns(HOST.into())];
    let leaf = generate::issue(&lp, &key.public_key(), &ca, &ca_key).expect("issue succeeds");
    let mut sc = ServerConfig::new(CertifiedKey { chain: vec![leaf], key });
    sc.ticket_key = Some(Secret(vec![3; 32]));
    let mut cc = ClientConfig::new(Trust::anchors(vec![ca]));
    cc.alpn = vec![b"a".to_vec()];
    (Arc::new(cc), Arc::new(sc))
}

fn client(cc: &Arc<ClientConfig>) -> Connection {
    Connection::client(cc.clone(), Some(HOST), NOW_MS, &[4; 48], None).expect("client succeeds")
}

fn connected() -> (Connection, Connection) {
    let (cc, sc) = configs();
    let mut c = client(&cc);
    let mut s = Connection::server(sc, NOW_MS, &[5; 48]);
    for _ in 0..6 {
        let a = c.take_output();
        s.feed(&a).expect("feed succeeds");
        let b = s.take_output();
        c.feed(&b).expect("feed succeeds");
    }
    assert_eq!(c.state, State::Connected);
    (c, s)
}

fn local(a: Alert) -> Result<(), Error> {
    Err(Error::Local(a))
}

/// Calls the client's handshake handler with a crafted message.
fn to_client(c: &mut Connection, typ: u8, body: &[u8]) -> Result<(), Error> {
    let msg = msgs::frame(typ, body);
    c.client_message(typ, body, &msg)
}

fn to_server(s: &mut Connection, typ: u8, body: &[u8]) -> Result<(), Error> {
    let msg = msgs::frame(typ, body);
    s.server_message(typ, body, &msg)
}

fn server_hello(c: &Connection, random: [u8; 32], e: Extensions) -> Vec<u8> {
    let sh = ServerHello { random, session_id: c.session_id.clone(), suite: 0x1301, extensions: e };
    msgs::unframe(&sh.encode()).expect("unframe succeeds").1.to_vec()
}

fn hrr_ext(group: u16, cookie: Option<&[u8]>) -> Extensions {
    let mut e = Extensions::default();
    e.push(ext::SUPPORTED_VERSIONS, vec![3, 4]);
    e.push(ext::KEY_SHARE, group.to_be_bytes().to_vec());
    if let Some(ck) = cookie {
        let mut v = Vec::new();
        put_vec16(&mut v, ck);
        e.push(ext::COOKIE, v);
    }
    e
}

#[test]
fn client_rejects_bad_server_hellos() {
    let (cc, _) = configs();
    // A retry with a cookie: the second hello echoes it; a second retry is unexpected.
    let mut c = client(&cc);
    let body =
        server_hello(&c, msgs::HRR_RANDOM, hrr_ext(NamedGroup::Secp256r1.code(), Some(b"ck")));
    assert_eq!(to_client(&mut c, hs::SERVER_HELLO, &body), Ok(()));
    let out = c.take_output();
    assert!(out.windows(2).any(|w| w == b"ck"));
    assert_eq!(to_client(&mut c, hs::SERVER_HELLO, &body), local(Alert::UnexpectedMessage));
    // A retry for the group whose share was already sent.
    let mut c = client(&cc);
    let body = server_hello(&c, msgs::HRR_RANDOM, hrr_ext(NamedGroup::X25519.code(), None));
    assert_eq!(to_client(&mut c, hs::SERVER_HELLO, &body), local(Alert::IllegalParameter));
    // A session id that is not the client's.
    let mut c = client(&cc);
    let mut sh = ServerHello {
        random: [0; 32],
        session_id: vec![1],
        suite: 0x1301,
        extensions: hrr_ext(0x1d, None),
    };
    let body = msgs::unframe(&sh.encode()).expect("unframe succeeds").1.to_vec();
    assert_eq!(to_client(&mut c, hs::SERVER_HELLO, &body), local(Alert::IllegalParameter));
    // A pre_shared_key the client did not offer, and a server hello without supported_versions.
    sh.session_id = c.session_id.clone();
    let mut e = Extensions::default();
    e.push(ext::SUPPORTED_VERSIONS, vec![3, 4]);
    let eph = crate::kx::Ephemeral::generate(NamedGroup::X25519, &[9; KEY_RANDOM_LEN])
        .expect("generate succeeds");
    e.push(ext::KEY_SHARE, msgs::key_share_entry(0x1d, &eph.public()));
    e.push(ext::PRE_SHARED_KEY, vec![0, 0]);
    sh.extensions = e;
    let body = msgs::unframe(&sh.encode()).expect("unframe succeeds").1.to_vec();
    assert_eq!(to_client(&mut c, hs::SERVER_HELLO, &body), local(Alert::IllegalParameter));
    let mut c = client(&cc);
    let body = server_hello(&c, [0; 32], Extensions::default());
    assert_eq!(to_client(&mut c, hs::SERVER_HELLO, &body), local(Alert::ProtocolVersion));
    // Out of order.
    let mut c = client(&cc);
    assert_eq!(to_client(&mut c, hs::FINISHED, &[]), local(Alert::UnexpectedMessage));
}

#[test]
fn client_rejects_bad_flights() {
    let (cc, _) = configs();
    let mut c = client(&cc);
    c.suite = Some(CipherSuite::Aes128GcmSha256);
    c.state = State::ClientWaitCertOrRequest;
    let cr = |ctx: Vec<u8>, sigs: bool| {
        let mut e = Extensions::default();
        if sigs {
            e.push(ext::SIGNATURE_ALGORITHMS, msgs::u16_list(&[0x0403]));
        }
        msgs::unframe(&msgs::CertificateRequest { context: ctx, extensions: e }.encode())
            .expect("unframe succeeds")
            .1
            .to_vec()
    };
    assert_eq!(
        to_client(&mut c, hs::CERTIFICATE_REQUEST, &cr(vec![1], true)),
        local(Alert::IllegalParameter)
    );
    assert_eq!(
        to_client(&mut c, hs::CERTIFICATE_REQUEST, &cr(vec![], false)),
        local(Alert::IllegalParameter)
    );
    let cm = |ctx: Vec<u8>, entries: Vec<msgs::CertEntry>| {
        msgs::unframe(&msgs::CertificateMsg { context: ctx, entries }.encode())
            .expect("unframe succeeds")
            .1
            .to_vec()
    };
    let entry = msgs::CertEntry { der: vec![0x30, 0], extensions: Extensions::default() };
    assert_eq!(
        to_client(&mut c, hs::CERTIFICATE, &cm(vec![1], vec![entry.clone()])),
        local(Alert::DecodeError)
    );
    assert_eq!(to_client(&mut c, hs::CERTIFICATE, &cm(vec![], vec![])), local(Alert::DecodeError));
    assert_eq!(
        to_client(&mut c, hs::CERTIFICATE, &cm(vec![], vec![entry])),
        local(Alert::BadCertificate)
    );
    // EncryptedExtensions with an ALPN protocol the client did not offer.
    c.state = State::ClientWaitEncryptedExtensions;
    let mut list = Vec::new();
    put_vec8(&mut list, b"zz");
    let mut alpn = Vec::new();
    put_vec16(&mut alpn, &list);
    let mut e = Extensions::default();
    e.push(ext::ALPN, alpn);
    let mut body = Vec::new();
    e.write(&mut body);
    assert_eq!(to_client(&mut c, hs::ENCRYPTED_EXTENSIONS, &body), local(Alert::IllegalParameter));
    // A wrong Finished.
    c.state = State::ClientWaitFinished;
    c.hs_secrets = Some((Secret(vec![0; 32]), Secret(vec![0; 32])));
    assert_eq!(to_client(&mut c, hs::FINISHED, &[0; 32]), local(Alert::DecryptError));
}

#[test]
fn post_handshake_messages() {
    let (mut c, mut s) = connected();
    let nst = |lifetime: u32| {
        let t = msgs::NewSessionTicket {
            lifetime,
            age_add: 0,
            nonce: vec![],
            ticket: vec![1],
            extensions: Extensions::default(),
        };
        msgs::unframe(&t.encode()).expect("unframe succeeds").1.to_vec()
    };
    let n = c.take_tickets().len();
    assert_eq!(n, 1);
    c.handle_post_handshake(hs::NEW_SESSION_TICKET, &nst(0))
        .expect("handle_post_handshake succeeds");
    assert!(c.take_tickets().is_empty());
    assert_eq!(
        c.handle_post_handshake(hs::NEW_SESSION_TICKET, &nst(700_000)),
        local(Alert::IllegalParameter)
    );
    assert_eq!(c.handle_post_handshake(hs::KEY_UPDATE, &[2]), local(Alert::IllegalParameter));
    assert_eq!(
        s.handle_post_handshake(hs::NEW_SESSION_TICKET, &nst(1)),
        local(Alert::UnexpectedMessage)
    );
    // A KeyUpdate followed by part of another message in the same record.
    let mut out = Vec::new();
    let mut msg = msgs::frame(hs::KEY_UPDATE, &[0]);
    msg.extend_from_slice(&[hs::FINISHED, 0]);
    s.records.write(ContentType::Handshake, &msg, &mut out).expect("write succeeds");
    assert_eq!(c.feed(&out), local(Alert::UnexpectedMessage));
    assert!(c.is_failed());
    assert_eq!(c.close(), Ok(()));
}

#[test]
fn records_after_the_handshake() {
    let (mut c, mut s) = connected();
    // An empty application_data record is accepted.
    let mut out = Vec::new();
    s.records
        .write
        .as_mut()
        .expect("as_mut succeeds")
        .seal(ContentType::ApplicationData, &[], &mut out)
        .expect("seal succeeds");
    assert_eq!(c.feed(&out), Ok(()));
    // change_cipher_spec once connected.
    assert_eq!(c.feed(&[20, 3, 3, 0, 1, 1]), local(Alert::UnexpectedMessage));
    let (mut c, mut s) = connected();
    // A protected change_cipher_spec.
    let mut out = Vec::new();
    s.records.write(ContentType::ChangeCipherSpec, &[1], &mut out).expect("write succeeds");
    assert_eq!(c.feed(&out), local(Alert::UnexpectedMessage));
    // A protected change_cipher_spec while the handshake runs (RFC 8446, 5: only an unprotected
    // one is dropped).
    let (mut c, mut s) = connected();
    c.state = State::ClientWaitFinished;
    let mut out = Vec::new();
    s.records.write(ContentType::ChangeCipherSpec, &[1], &mut out).expect("write succeeds");
    assert_eq!(c.feed(&out), local(Alert::UnexpectedMessage));
    // Data after close_notify, and a fatal user_canceled.
    let (mut c, mut s) = connected();
    s.close().expect("close succeeds");
    let mut out = s.take_output();
    let mut late = Vec::new();
    s.records.write(ContentType::ApplicationData, b"late", &mut late).expect("write succeeds");
    out.extend_from_slice(&late);
    assert_eq!(c.feed(&out), local(Alert::UnexpectedMessage));
    let (mut c, mut s) = connected();
    let mut out = Vec::new();
    s.records.write(ContentType::Alert, &[2, 90], &mut out).expect("write succeeds");
    assert_eq!(c.feed(&out), Err(Error::Peer(Alert::UserCanceled)));
    // An inner plaintext longer than 2^14.
    let (mut c, mut s) = connected();
    let mut out = Vec::new();
    s.records
        .write
        .as_mut()
        .expect("as_mut succeeds")
        .seal(ContentType::ApplicationData, &vec![1; crate::record::MAX_PLAINTEXT + 1], &mut out)
        .expect("seal succeeds");
    assert_eq!(c.feed(&out), local(Alert::RecordOverflow));
}

fn hello(e: Extensions, suites: Vec<u16>, session_id: Vec<u8>) -> Vec<u8> {
    let ch = ClientHello {
        legacy_version: msgs::TLS12,
        random: [7; 32],
        session_id,
        suites,
        compression: vec![0],
        extensions: e,
    };
    msgs::unframe(&ch.encode()).expect("unframe succeeds").1.to_vec()
}

fn base_ext(group_share: Option<(u16, Vec<u8>)>, groups: &[u16]) -> Extensions {
    let mut e = Extensions::default();
    e.push(ext::SUPPORTED_VERSIONS, vec![2, 3, 4]);
    e.push(ext::SUPPORTED_GROUPS, msgs::u16_list(groups));
    e.push(ext::SIGNATURE_ALGORITHMS, msgs::u16_list(&[0x0807]));
    let mut ks = Vec::new();
    if let Some((g, k)) = group_share {
        ks = msgs::key_share_entry(g, &k);
    }
    let mut v = Vec::new();
    put_vec16(&mut v, &ks);
    e.push(ext::KEY_SHARE, v);
    e
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "one letter per side, record and key of the case"
)]
fn server_rejects_bad_client_hellos() {
    let (_, sc) = configs();
    let server = || Connection::server(sc.clone(), NOW_MS, &[5; 48]);
    let share = crate::kx::Ephemeral::generate(NamedGroup::Secp256r1, &[8; KEY_RANDOM_LEN])
        .expect("generate succeeds")
        .public();
    // A retry with an empty session id sends no change_cipher_spec; the second hello may not
    // change the suite.
    let mut s = server();
    let b = hello(base_ext(None, &[0x17]), vec![0x1301, 0x1302], vec![]);
    assert_eq!(to_server(&mut s, hs::CLIENT_HELLO, &b), Ok(()));
    assert!(!s.take_output().windows(6).any(|w| w == [20, 3, 3, 0, 1, 1]));
    let b2 = hello(base_ext(Some((0x17, share.clone())), &[0x17]), vec![0x1302], vec![]);
    assert_eq!(to_server(&mut s, hs::CLIENT_HELLO, &b2), local(Alert::IllegalParameter));
    // The second hello still without the requested share.
    let mut s = server();
    assert_eq!(to_server(&mut s, hs::CLIENT_HELLO, &b), Ok(()));
    assert_eq!(to_server(&mut s, hs::CLIENT_HELLO, &b), local(Alert::IllegalParameter));
    // Duplicate key shares, a bad SNI, and a client certificate with a context.
    let mut e = base_ext(None, &[0x17]);
    let mut dup = msgs::key_share_entry(0x17, &share);
    dup.extend_from_slice(&msgs::key_share_entry(0x17, &share));
    let mut v = Vec::new();
    put_vec16(&mut v, &dup);
    e.0.retain(|(t, _)| *t != ext::KEY_SHARE);
    e.push(ext::KEY_SHARE, v);
    assert_eq!(
        to_server(&mut server(), hs::CLIENT_HELLO, &hello(e, vec![0x1301], vec![])),
        local(Alert::IllegalParameter)
    );
    for sni in [vec![1u8, 0, 1, b'x', 0, 0, 0], vec![0, 0, 0]] {
        let mut list = Vec::new();
        put_vec16(&mut list, &sni);
        let mut e = base_ext(Some((0x17, share.clone())), &[0x17]);
        e.0.insert(0, (ext::SERVER_NAME, list));
        let r = to_server(&mut server(), hs::CLIENT_HELLO, &hello(e, vec![0x1301], vec![]));
        assert_eq!(r, local(Alert::DecodeError));
    }
    let mut s = server();
    s.state = State::ServerWaitCertificate;
    let cm = msgs::CertificateMsg { context: vec![1], entries: vec![] };
    assert_eq!(
        to_server(
            &mut s,
            hs::CERTIFICATE,
            msgs::unframe(&cm.encode()).expect("unframe succeeds").1
        ),
        local(Alert::IllegalParameter)
    );
    let _ = ClientAuth::None;
}

#[test]
fn server_ignores_unusable_tickets() {
    let (cc, sc) = configs();
    // Resumption with a PSK whose modes lack psk_dhe_ke, with a corrupt ticket and with a ticket
    // of another hash: full handshakes.
    let share = crate::kx::Ephemeral::generate(NamedGroup::X25519, &[8; KEY_RANDOM_LEN])
        .expect("generate succeeds")
        .public();
    let psk_hello = |modes: Vec<u8>, identity: Vec<u8>| {
        let mut e = base_ext(Some((0x1d, share.clone())), &[0x1d]);
        e.push(ext::PSK_KEY_EXCHANGE_MODES, modes);
        let offered =
            msgs::OfferedPsks { identities: vec![(identity, 0)], binders: vec![vec![0; 32]] };
        e.push(ext::PRE_SHARED_KEY, offered.encode());
        hello(e, vec![0x1301], vec![1; 32])
    };
    for (modes, id) in
        [(vec![1, 0], vec![1; 40]), (vec![1, 1], vec![1; 5]), (vec![1, 1], vec![1; 40])]
    {
        let mut s = Connection::server(sc.clone(), NOW_MS, &[5; 48]);
        assert_eq!(to_server(&mut s, hs::CLIENT_HELLO, &psk_hello(modes, id)), Ok(()));
        assert!(!s.is_resumed());
    }
    // A real ticket used with a SHA-384 suite, and after it expired.
    let (mut c, _) = connected();
    let ticket = c.take_tickets().remove(0);
    let mut s384 = (*sc).clone();
    s384.suites = vec![CipherSuite::Aes256GcmSha384];
    let mut c384 = (*cc).clone();
    c384.suites = vec![CipherSuite::Aes256GcmSha384, CipherSuite::Aes128GcmSha256];
    let mut c2 =
        Connection::client(Arc::new(c384), Some(HOST), NOW_MS, &[4; 48], Some(ticket.clone()))
            .expect("client succeeds");
    let mut s2 = Connection::server(Arc::new(s384), NOW_MS, &[5; 48]);
    s2.feed(&c2.take_output()).expect("feed succeeds");
    assert!(!s2.is_resumed());
    let late = NOW_MS + 2 * 86_400_000;
    let mut c3 = Connection::client(cc.clone(), Some(HOST), NOW_MS, &[4; 48], Some(ticket))
        .expect("client succeeds");
    let mut s3 = Connection::server(sc.clone(), late, &[5; 48]);
    s3.feed(&c3.take_output()).expect("feed succeeds");
    assert!(!s3.is_resumed());
    let _ = c2.read();
}
