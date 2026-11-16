//! The administration port and its client on the engine's shards (023 T057, FR-022; 007 FR-020,
//! 013 FR-017): one request per connection with its limits, a handler slower than the request's
//! deadline still answered, TLS chosen by the first byte with each caller named by its client
//! certificate, a failed handshake losing only its own answer, and a client that refuses an
//! answer that is not HTTP.
#![cfg(target_os = "linux")]

mod engine_util;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use engine_util::{off_shard, on_engine};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb_server::admin_http::shard::{AdminPort, Handler, MakeHandler};
use teseladb_server::admin_http::{
    AdminTls, Caller, HttpRequest, HttpResponse, IO_TIMEOUT, Method, request, request_with,
};
use teseladb_server::pki;
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_tls::{CertifiedKey, ClientAuth, ClientConfig, ServerConfig, Trust, Version};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::oid;

/// A handler that answers the caller and the request.
fn echo() -> MakeHandler {
    Arc::new(|| -> Handler {
        Rc::new(|r: HttpRequest, c: Caller| {
            make_ready_future(HttpResponse::text(200, &format!("{:?} {} {c:?}", r.method, r.path)))
        })
    })
}

/// Runs `client` against an administration port of `make` with `tls`, then stops it.
fn with_port<T: Send + 'static>(
    make: MakeHandler,
    tls: Option<AdminTls>,
    client: impl FnOnce(SocketAddr) -> T + Send + 'static,
) -> T {
    on_engine(move || {
        make_coroutine(async move {
            let at = SocketAddr::from(([127, 0, 0, 1], 0));
            let port = AdminPort::start(at, make, tls).await?;
            let at = port.local_addr();
            let out = off_shard(move || client(at)).await;
            port.stop().await?;
            out
        })
    })
}

#[test]
fn the_port_answers_each_request_and_stops() {
    on_engine(|| {
        make_coroutine(async {
            let at = SocketAddr::from(([127, 0, 0, 1], 0));
            let port = AdminPort::start(at, echo(), None).await?;
            let at = port.local_addr();
            off_shard(move || answers(at)).await?;
            port.stop().await?;
            // Stopped: nobody answers.
            let a = at.to_string();
            off_shard(move || assert!(request(&a, Method::Get, "/x", b"").is_err())).await
        })
    });
}

/// The answers of a port of [`echo`] at `at`.
fn answers(at: SocketAddr) {
    {
        let a = at.to_string();
        assert_eq!(
            request(&a, Method::Get, "/healthz", b"").unwrap(),
            (200, b"Get /healthz Open".to_vec())
        );
        assert_eq!(
            request(&a, Method::Post, "/a?b=1", b"{}").unwrap(),
            (200, b"Post /a Open".to_vec())
        );
        // A request the port cannot parse gets its error.
        let mut s = TcpStream::connect(at).unwrap();
        s.write_all(b"BREW /x HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 405 "), "{answer}");
        // A client that closes before sending a whole request gets nothing.
        drop(TcpStream::connect(at).unwrap());
        assert_eq!(request(&a, Method::Get, "/x", b"").unwrap().0, 200);
    }
}

/// A client that sends half a request is closed once [`IO_TIMEOUT`] passes, on the engine's
/// timers.
#[test]
fn a_request_that_never_ends_is_closed_after_the_timeout() {
    let waited = with_port(echo(), None, |at| {
        let mut s = TcpStream::connect(at).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let start = std::time::Instant::now();
        s.write_all(b"GET /x HTTP/1.1\r\nHost: x\r\n").unwrap();
        let mut buf = [0u8; 64];
        assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "an answer came");
        start.elapsed()
    });
    assert!(waited >= IO_TIMEOUT && waited < IO_TIMEOUT * 2, "{waited:?}");
}

#[test]
fn a_client_waits_for_an_answer_slower_than_the_connections_timeout() {
    // A handler may wait for a core and then for Group 0 (`range split`) longer than a
    // connection has to send its request: the client still gets the answer.
    let slow: MakeHandler = Arc::new(|| -> Handler {
        Rc::new(|_: HttpRequest, _: Caller| -> Future<HttpResponse> {
            make_coroutine(async {
                sleep_steady(IO_TIMEOUT + Duration::from_millis(300)).await?;
                Ok(HttpResponse::text(200, "late"))
            })
        })
    });
    with_port(slow, None, |at| {
        assert_eq!(
            request(&at.to_string(), Method::Get, "/x", b"").unwrap(),
            (200, b"late".to_vec())
        );
    });
}

fn random(b: &mut [u8]) -> io::Result<()> {
    teseladb_sys::fill_random(b)
}

fn identity(p: &pki::Pki) -> CertifiedKey {
    CertifiedKey { chain: p.node_chain.clone(), key: p.node_key.clone() }
}

#[test]
fn over_tls_each_caller_is_named_by_its_certificate() {
    let base = std::env::temp_dir().join(format!("teseladb-admin-shard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
    let params = pki::PkiParams { now, node_names: vec![GeneralName::Ip(vec![127, 0, 0, 1])] };
    let cluster = pki::load_or_create(&base.join("cluster"), &params, &mut random).unwrap();
    // Another CA, trusted for clients like a `--tlsCAFile`: its certificates are users.
    let users = pki::load_or_create(&base.join("users"), &params, &mut random).unwrap();
    let mut server = ServerConfig::new(identity(&cluster));
    server.min_version = Version::Tls12;
    server.client_auth = ClientAuth::Optional;
    server.trust = Trust::anchors(vec![cluster.ca.clone(), users.ca.clone()]);
    let tls = AdminTls {
        port: TlsPort::new(TlsMode::RequireTls, Some(server)),
        ca: cluster.ca.clone(),
        node: cluster.node_chain[0].subject.clone(),
    };
    let named: MakeHandler = Arc::new(|| -> Handler {
        Rc::new(|_: HttpRequest, c: Caller| {
            make_ready_future(HttpResponse::text(200, &format!("{c:?}")))
        })
    });
    // A client certificate of the cluster's CA with another organizational unit is a user.
    let key = teseladb_x509::PrivateKey::ed25519(&[9; 32]);
    let name = teseladb_x509::Name::build(&[
        (oid::ORGANIZATION, "TeselaDB"),
        (oid::ORGANIZATIONAL_UNIT, "operators"),
        (oid::COMMON_NAME, "ops"),
    ]);
    let p = teseladb_x509::generate::CertificateParams::node(
        [3; 16],
        name.clone(),
        now - 60,
        now + 600,
    );
    let cert = teseladb_x509::generate::issue(&p, &key.public_key(), &cluster.ca, &cluster.ca_key)
        .unwrap();
    let mut operator = ClientConfig::new(Trust::anchors(vec![cluster.ca.clone()]));
    operator.identity = Some(CertifiedKey { chain: vec![cert], key });
    let ca = cluster.ca.clone();
    let client = move |who: Option<CertifiedKey>| {
        let mut c = ClientConfig::new(Trust::anchors(vec![ca.clone()]));
        c.identity = who;
        Some(Arc::new(c))
    };
    let (member, user) = (identity(&cluster), identity(&users));
    let user_name = users.node_chain[0].subject.x509_user_name();
    let operator_name = name.x509_user_name();
    with_port(named, Some(tls.clone()), move |at| {
        let a = at.to_string();
        let ask = |tls| {
            let (status, body) = request_with(&a, tls, Method::Get, "/metrics", b"").unwrap();
            assert_eq!(status, 200);
            String::from_utf8(body).unwrap()
        };
        assert_eq!(ask(client(Some(member))), "Member");
        assert_eq!(ask(client(Some(user))), format!("{:?}", Caller::User(user_name)));
        assert_eq!(ask(Some(Arc::new(operator))), format!("{:?}", Caller::User(operator_name)));
        assert_eq!(ask(client(None)), "Anonymous");
        // Without TLS the request still gets an answer, as an anonymous caller.
        assert_eq!(ask(None), "Anonymous");
        // A server whose certificate does not name the host is refused.
        let wrong = format!("localhost:{}", at.port());
        assert!(request_with(&wrong, client(None), Method::Get, "/", b"").is_err());
        // A handshake that fails and a peer that closes during it only lose their own answer.
        let mut s = TcpStream::connect(at).unwrap();
        s.write_all(&[22, 3, 1, 0, 1, 0]).unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        let mut s = TcpStream::connect(at).unwrap();
        s.write_all(&[22, 3, 1]).unwrap();
        drop(s);
        assert_eq!(ask(client(None)), "Anonymous");
    });
    // A port whose TLS is disabled reads a handshake as a plain request, which it is not.
    let plain_tls = AdminTls { port: TlsPort::new(TlsMode::Disabled, None), ..tls };
    let named: MakeHandler = Arc::new(|| -> Handler {
        Rc::new(|_: HttpRequest, c: Caller| {
            make_ready_future(HttpResponse::text(200, &format!("{c:?}")))
        })
    });
    let ca = cluster.ca.clone();
    with_port(named, Some(plain_tls), move |at| {
        let a = at.to_string();
        assert_eq!(request(&a, Method::Get, "/", b"").unwrap().1, b"Anonymous");
        let c = ClientConfig::new(Trust::anchors(vec![ca]));
        assert!(request_with(&a, Some(Arc::new(c)), Method::Get, "/", b"").is_err());
    });
    let _ = std::fs::remove_dir_all(&base);
}

/// The client refuses an answer that is not HTTP.
#[test]
fn an_answer_that_is_not_http_is_refused() {
    let raw = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let raw_at = raw.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for (i, mut s) in raw.incoming().flatten().enumerate() {
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            let answer: &[u8] = if i == 0 { b"garbage" } else { b"\xff\xfe\r\n\r\n" };
            let _ = s.write_all(answer);
        }
    });
    // The client runs on the engine, which must keep running while it waits.
    on_engine(move || {
        off_shard(move || {
            for _ in 0..2 {
                let e = request(&raw_at, Method::Get, "/", b"").unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidData);
            }
        })
    });
}

/// Which caller a validated chain names: the cluster's members by the CA and the attributes of
/// the node's subject, anyone else as a user.
#[test]
fn a_caller_is_a_member_only_with_the_clusters_ca_and_attributes() {
    let base = std::env::temp_dir().join(format!("teseladb-admin-caller-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let names = vec![GeneralName::Ip(vec![127, 0, 0, 1])];
    let params = pki::PkiParams { now: 1_800_000_000, node_names: names };
    let cluster = pki::load_or_create(&base.join("cluster"), &params, &mut random).unwrap();
    let tls = AdminTls {
        port: TlsPort::new(TlsMode::Disabled, None),
        ca: cluster.ca.clone(),
        node: cluster.node_chain[0].subject.clone(),
    };
    assert_eq!(tls.caller(&cluster.node_chain), Caller::Member);
    assert_eq!(tls.caller(&[]), Caller::Anonymous);
    // A certificate of another CA with the cluster CA's name is not a node.
    let impostor = pki::load_or_create(&base.join("impostor"), &params, &mut random).unwrap();
    let same_name = AdminTls { ca: impostor.ca.clone(), ..tls.clone() };
    assert_eq!(
        same_name.caller(&cluster.node_chain),
        Caller::User(cluster.node_chain[0].subject.x509_user_name())
    );
    // A certificate that names the cluster's CA as issuer but another key signed: a user.
    let other_key = teseladb_x509::PrivateKey::ed25519(&[4; 32]);
    let mut forged_params = teseladb_x509::generate::CertificateParams::node(
        [7; 16],
        cluster.ca.subject.clone(),
        0,
        i64::MAX / 2,
    );
    forged_params.ca = Some(None);
    let forged_ca = teseladb_x509::generate::self_signed(&forged_params, &other_key).unwrap();
    let leaf_params = teseladb_x509::generate::CertificateParams::node(
        [8; 16],
        cluster.node_chain[0].subject.clone(),
        0,
        i64::MAX / 2,
    );
    let forged = teseladb_x509::generate::issue(
        &leaf_params,
        &cluster.node_key.public_key(),
        &forged_ca,
        &other_key,
    )
    .unwrap();
    assert_eq!(
        tls.caller(std::slice::from_ref(&forged)),
        Caller::User(forged.subject.x509_user_name())
    );
    let _ = std::fs::remove_dir_all(&base);
}
