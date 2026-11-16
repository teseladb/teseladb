//! The LDAP client on the engine (023 FR-024, T064): logins and groups with their cache, each
//! server tried in turn, LDAPS, the deadline of `--ldapTimeoutMS` on the engine's timers, and the
//! futures a shard awaits.

#![cfg(target_os = "linux")]

#[path = "common/blocking_tls.rs"]
mod blocking_tls;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use blocking_tls::{Peer, TlsStream};
use teseladb_auth::ldap_ber::{self, tag};
use teseladb_auth::roles::ADMIN_DB;
use teseladb_auth::users::RoleName;
use teseladb_server::cli::Password;
use teseladb_server::ldap_client::{LdapOpts, LdapServer, SharedLdap};
use teseladb_server::outbound::{self, test_engine};
use teseladb_tls::{CertifiedKey, ServerConfig, Trust};
use teseladb_wire::commands::LdapLogin;
use teseladb_x509::ext::GeneralName;

const NONE: &str = "none";

/// Answers the LDAP messages of one connection: `uid=<u>,ou=people` binds with `<u>-pw`; a
/// search whose request holds `uid=jane` finds `cn=readers`.
fn answer(mut link: Peer, searches: &AtomicUsize) {
    let mut buf = Vec::new();
    while let Ok(data) = link.recv() {
        if data.is_empty() {
            return;
        }
        buf.extend_from_slice(&data);
        while let Ok(Some((_, content, rest))) = ldap_ber::read(&buf) {
            let used = buf.len() - rest.len();
            let (id, op) = ldap_ber::expect(content, tag::INTEGER).unwrap();
            let (t, body, _) = ldap_ber::read_whole(op).unwrap();
            let reply = |op: Vec<u8>| {
                let mut m = ldap_ber::element(tag::INTEGER, id);
                m.extend_from_slice(&op);
                ldap_ber::element(tag::SEQUENCE, &m)
            };
            let result = |code: u8| {
                let mut r = ldap_ber::element(tag::ENUMERATED, &[code]);
                ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
                ldap_ber::put(&mut r, tag::OCTET_STRING, b"");
                r
            };
            if t == 0x60 {
                let parts = ldap_ber::elements(body).unwrap();
                let dn = String::from_utf8_lossy(parts[1].1).into_owned();
                let user = dn.strip_prefix("uid=").and_then(|d| d.strip_suffix(",ou=people"));
                let ok = user.is_some_and(|u| parts[2].1 == format!("{u}-pw").as_bytes());
                let _ =
                    link.send(&reply(ldap_ber::element(0x61, &result(if ok { 0 } else { 49 }))));
            }
            if t == 0x63 {
                searches.fetch_add(1, Ordering::SeqCst);
                if body.windows(8).any(|w| w == b"uid=jane") {
                    let mut e = ldap_ber::element(tag::OCTET_STRING, b"cn=readers");
                    ldap_ber::put(&mut e, tag::SEQUENCE, &[]);
                    let _ = link.send(&reply(ldap_ber::element(0x64, &e)));
                }
                let _ = link.send(&reply(ldap_ber::element(0x65, &result(0))));
            }
            buf.drain(..used);
        }
    }
}

/// An LDAP server on a thread of the test, over TLS with `tls`; counts the searches.
fn server(tls: Option<Arc<ServerConfig>>, searches: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { return };
            let (tls, searches) = (tls.clone(), searches.clone());
            std::thread::spawn(move || {
                let link = match tls {
                    None => Peer::Plain(sock),
                    Some(c) => match TlsStream::server(sock, c, &[]) {
                        Ok(s) => Peer::Tls(Box::new(s)),
                        Err(_) => return,
                    },
                };
                answer(link, &searches);
            });
        }
    });
    port
}

fn opts(port: u16) -> LdapOpts {
    LdapOpts {
        servers: Some(format!("127.0.0.1:{port}")),
        transport: Some(NONE.into()),
        mapping: Some("[{match: '(.+)', substitution: 'uid={0},ou=people'}]".into()),
        ..LdapOpts::default()
    }
}

fn roles(names: &[&str]) -> LdapLogin {
    LdapLogin::Authorized(names.iter().map(|n| RoleName::new(*n, ADMIN_DB)).collect())
}

fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn plain_logins_and_groups_with_their_cache() {
    test_engine::install();
    let searches = Arc::new(AtomicUsize::new(0));
    let port = server(None, searches.clone());
    let s = LdapServer::from_opts(&opts(port)).unwrap().unwrap();
    assert!(format!("{s:?}").contains("127.0.0.1"));
    assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::LoggedIn));
    assert_eq!(s.login("jane", b"wrong"), Ok(LdapLogin::Refused));
    assert_eq!(s.login("jane", b""), Ok(LdapLogin::Refused));
    let o = LdapOpts {
        authz_query_template: Some("ou=groups??one?(member={USER})".into()),
        cache_interval_secs: Some(1),
        ..opts(port)
    };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    assert_eq!(s.login("jane", b"jane-pw"), Ok(roles(&["cn=readers"])));
    assert_eq!(s.login("lone", b"lone-pw"), Ok(roles(&[])));
    assert_eq!(searches.load(Ordering::SeqCst), 2);
    // Kept for the interval (also by a clone), then searched again; and forgotten on invalidation.
    assert_eq!(s.clone().login("jane", b"jane-pw"), Ok(roles(&["cn=readers"])));
    assert_eq!(searches.load(Ordering::SeqCst), 2);
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(s.login("jane", b"jane-pw"), Ok(roles(&["cn=readers"])));
    assert_eq!(searches.load(Ordering::SeqCst), 3);
    s.invalidate();
    assert_eq!(s.login("jane", b"jane-pw"), Ok(roles(&["cn=readers"])));
    assert_eq!(searches.load(Ordering::SeqCst), 4);
    // A wrong password is refused without searching.
    assert_eq!(s.login("jane", b"bad"), Ok(LdapLogin::Refused));
    let shared = SharedLdap(Arc::new(s));
    assert_eq!(shared, shared.clone());
}

/// The groups of a X.509 authentication user (013 T045): only with the authorization template,
/// without a bind of the user, kept like those of a PLAIN login; a dead server is an error.
#[test]
fn groups_of_a_user_that_logged_in_another_way() {
    test_engine::install();
    let searches = Arc::new(AtomicUsize::new(0));
    let port = server(None, searches.clone());
    let s = LdapServer::from_opts(&opts(port)).unwrap().unwrap();
    assert_eq!(s.groups("jane"), Ok(None));
    let o = LdapOpts {
        authz_query_template: Some("ou=groups??one?(member={USER})".into()),
        ..opts(port)
    };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    let readers = Some(vec![RoleName::new("cn=readers", ADMIN_DB)]);
    assert_eq!(s.groups("jane"), Ok(readers.clone()));
    assert_eq!(s.groups("lone"), Ok(Some(Vec::new())));
    assert_eq!(searches.load(Ordering::SeqCst), 2);
    assert_eq!(s.groups("jane"), Ok(readers));
    assert_eq!(searches.load(Ordering::SeqCst), 2, "kept in the cache");
    // A dead server first: the next one answers; with only dead ones, an error.
    let fresh = LdapServer::from_opts(&LdapOpts {
        servers: Some(format!("127.0.0.1:{},127.0.0.1:{port}", dead_port())),
        ..o.clone()
    })
    .unwrap()
    .unwrap();
    assert_eq!(fresh.groups("jane"), Ok(Some(vec![RoleName::new("cn=readers", ADMIN_DB)])));
    let s = LdapServer::from_opts(&LdapOpts {
        servers: Some(format!("127.0.0.1:{}", dead_port())),
        ..o
    })
    .unwrap()
    .unwrap();
    assert!(s.groups("jane").is_err());
}

#[test]
fn each_server_is_tried_and_errors_are_named() {
    test_engine::install();
    let port = server(None, Arc::new(AtomicUsize::new(0)));
    let dead = dead_port();
    let o = LdapOpts { servers: Some(format!("127.0.0.1:{dead},127.0.0.1:{port}")), ..opts(port) };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::LoggedIn));
    let o = LdapOpts { servers: Some(format!("127.0.0.1:{dead}")), ..opts(port) };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    let e = s.login("jane", b"jane-pw").unwrap_err();
    assert!(e.starts_with(&format!("127.0.0.1:{dead}: ")), "{e}");
    // A server that closes at once.
    let closing = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = closing.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in closing.incoming() {
            drop(c);
        }
    });
    let o = LdapOpts { servers: Some(format!("127.0.0.1:{at}")), ..opts(port) };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    assert!(s.login("jane", b"jane-pw").is_err());
    let o = LdapOpts { servers: Some("no-such-host.invalid:1".into()), ..opts(port) };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    assert!(s.login("jane", b"jane-pw").unwrap_err().starts_with("no-such-host.invalid:1: "));
    // A mapping with a search needs the query user's bind first; a user no rule maps.
    let o = LdapOpts {
        mapping: Some("[{match: 'x(.+)', ldapQuery: 'dc=x??sub?(uid={0})'}]".into()),
        ..opts(port)
    };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::Refused));
}

/// A server that accepts and never answers: the login fails at `--ldapTimeoutMS`, measured on
/// the engine's timers, without holding a thread of the node.
#[test]
fn a_silent_server_times_out() {
    test_engine::install();
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let o = LdapOpts { timeout_ms: Some(300), ..opts(port) };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    let began = Instant::now();
    let e = s.login("jane", b"jane-pw").unwrap_err();
    let took = began.elapsed();
    assert!(e.contains("timed out"), "{e}");
    assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(5), "{took:?}");
    drop(silent);
}

#[test]
fn ldaps_with_the_servers_certificate() {
    test_engine::install();
    let dir = std::env::temp_dir().join(format!("teseladb-ldaps-engine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
    let params = teseladb_server::pki::PkiParams {
        now,
        node_names: vec![GeneralName::Dns("localhost".into())],
    };
    let pki =
        teseladb_server::pki::load_or_create(&dir, &params, &mut |b| teseladb_sys::fill_random(b))
            .unwrap();
    let identity = CertifiedKey { chain: pki.node_chain.clone(), key: pki.node_key.clone() };
    let port = server(Some(Arc::new(ServerConfig::new(identity))), Arc::new(AtomicUsize::new(0)));
    // LDAPS by default: the server's certificate names `localhost`.
    let o = LdapOpts { transport: None, servers: Some(format!("localhost:{port}")), ..opts(port) };
    let ca = pki.ca.clone();
    let s = LdapServer::from_opts_with(&o, move || Ok(Trust::anchors(vec![ca]))).unwrap().unwrap();
    assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::LoggedIn));
    // A server whose certificate the client does not trust.
    let s = LdapServer::from_opts_with(&o, || Ok(Trust::anchors(Vec::new()))).unwrap().unwrap();
    assert!(s.login("jane", b"jane-pw").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mapping_with_a_search_binds_the_query_user_first() {
    test_engine::install();
    let searches = Arc::new(AtomicUsize::new(0));
    let port = server(None, searches.clone());
    // The query user binds, the search maps `jane` to `cn=readers`, whose bind with `jane-pw`
    // the server refuses.
    let o = LdapOpts {
        mapping: Some("[{match: '(.+)', ldapQuery: 'dc=x??sub?(uid={0})'}]".into()),
        query_user: Some("uid=q,ou=people".into()),
        query_password: Some(Password::from("q-pw")),
        ..opts(port)
    };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::Refused));
    assert_eq!(searches.load(Ordering::SeqCst), 1);
}

/// A shard awaits the client's futures; it cannot block on them.
#[test]
fn a_shard_awaits_the_futures() {
    test_engine::install();
    let port = server(None, Arc::new(AtomicUsize::new(0)));
    let o = LdapOpts {
        authz_query_template: Some("ou=groups??one?(member={USER})".into()),
        ..opts(port)
    };
    let s = LdapServer::from_opts(&o).unwrap().unwrap();
    let (a, b) = (s.clone(), s.clone());
    assert_eq!(
        outbound::call(move || a.login_on_shard("jane", b"jane-pw")),
        Ok(roles(&["cn=readers"]))
    );
    let readers = Some(vec![RoleName::new("cn=readers", ADMIN_DB)]);
    assert_eq!(outbound::call(move || b.groups_on_shard("jane")), Ok(readers));
    let on_shard = outbound::call(move || {
        teseladb::engine::core::future::make_ready_future(s.login("jane", b"jane-pw"))
    });
    assert_eq!(on_shard, Ok(Err(outbound::ON_SHARD.to_owned())));
}
