//! The node's LDAP server (013 FR-025): RefDb Enterprise's options `--ldapServers`,
//! `--ldapTransportSecurity`, `--ldapBindMethod`, `--ldapQueryUser`, `--ldapQueryPassword`,
//! `--ldapUserToDNMapping` and `--ldapTimeoutMS`, and the connection a PLAIN login opens to run
//! the conversation of [`teseladb_auth::ldap::Login`]: plain TCP with `none`, LDAPS (TLS from
//! the first byte) with `tls`, trusting the system's certificate authorities. Each server of the
//! list is tried in turn until one answers.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use teseladb_auth::ldap::{AuthzTemplate, LdapError, Login, Mapping, Step};
use teseladb_auth::roles::ADMIN_DB;
use teseladb_auth::users::RoleName;
use teseladb_tls::{ClientConfig, Trust};
use teseladb_wire::commands::LdapLogin;

use crate::cli::Password;
use crate::tls_stream::TlsStream;

/// LDAP's port without TLS.
const LDAP_PORT: u16 = 389;
/// LDAPS's port.
const LDAPS_PORT: u16 = 636;
/// `--ldapTimeoutMS` by default, as RefDb.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const NONE: &str = "none";
const TLS: &str = "tls";
const SIMPLE: &str = "simple";

/// The options as given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LdapOpts {
    /// `--ldapServers`: `host[:port]`, comma separated.
    pub servers: Option<String>,
    /// `--ldapTransportSecurity`: `tls` (default) or `none`.
    pub transport: Option<String>,
    /// `--ldapBindMethod`: `simple` (default).
    pub bind_method: Option<String>,
    /// `--ldapQueryUser`.
    pub query_user: Option<String>,
    /// `--ldapQueryPassword`.
    pub query_password: Option<Password>,
    /// `--ldapUserToDNMapping`.
    pub mapping: Option<String>,
    /// `--ldapTimeoutMS`.
    pub timeout_ms: Option<u64>,
    /// `--ldapAuthzQueryTemplate`: roles from the user's LDAP groups (013 FR-026).
    pub authz_query_template: Option<String>,
    /// `--setParameter ldapUserCacheInvalidationInterval`: seconds the groups are kept.
    pub cache_interval_secs: Option<u64>,
    /// `--tlsFIPSMode`: the TLS to the LDAP server keeps only FIPS-approved algorithms.
    pub fips: bool,
}

/// `ldapUserCacheInvalidationInterval` by default, as RefDb: 30 seconds.
pub const DEFAULT_CACHE_INTERVAL_SECS: u64 = 30;

/// The node's LDAP server, ready to log users in.
pub struct LdapServer {
    hosts: Vec<(String, u16)>,
    tls: Option<Arc<ClientConfig>>,
    query: Option<(String, Vec<u8>)>,
    mapping: Mapping,
    timeout: Duration,
    authz: Option<AuthzTemplate>,
    /// The groups of each user name and when they were read.
    cache: Mutex<HashMap<String, (Instant, Vec<String>)>>,
    cache_interval: Duration,
}

impl std::fmt::Debug for LdapServer {
    /// Without the query password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LdapServer")
            .field("hosts", &self.hosts)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

fn host_port(text: &str, default: u16) -> Result<(String, u16), String> {
    let bad = || format!("--ldapServers: bad server `{text}`");
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => {
            let (h, after) = rest.split_once(']').ok_or_else(bad)?;
            (
                h,
                if after.is_empty() {
                    None
                } else {
                    Some(after.strip_prefix(':').ok_or_else(bad)?)
                },
            )
        }
        None => match text.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (text, None),
        },
    };
    let port = match port {
        None => default,
        Some(p) => p.parse().map_err(|_| bad())?,
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok((host.to_owned(), port))
}

impl LdapServer {
    /// The server the options describe; `None` without `--ldapServers`.
    pub fn from_opts(o: &LdapOpts) -> Result<Option<LdapServer>, String> {
        LdapServer::from_opts_with(o, crate::https::system_trust)
    }

    /// [`LdapServer::from_opts`] with the authorities of `--ldapTransportSecurity tls` from
    /// `system_trust`.
    fn from_opts_with(
        o: &LdapOpts,
        system_trust: fn() -> Result<Trust, String>,
    ) -> Result<Option<LdapServer>, String> {
        let Some(servers) = &o.servers else {
            let given = o.query_user.is_some()
                || o.mapping.is_some()
                || o.transport.is_some()
                || o.authz_query_template.is_some();
            return if given {
                Err("the LDAP options need --ldapServers".into())
            } else {
                Ok(None)
            };
        };
        let tls = match o.transport.as_deref().unwrap_or(TLS) {
            TLS => {
                let trust =
                    system_trust().map_err(|e| format!("--ldapTransportSecurity tls: {e}"))?;
                // TLS 1.2 or 1.3, as `refnode` reaches LDAP servers through OpenSSL.
                let config = ClientConfig {
                    min_version: teseladb_tls::Version::Tls12,
                    ..ClientConfig::new(trust)
                };
                Some(Arc::new(if o.fips { config.fips() } else { config }))
            }
            NONE => None,
            other => {
                return Err(format!(
                    "--ldapTransportSecurity: expected tls or none, got `{other}`"
                ));
            }
        };
        if let Some(m) = o.bind_method.as_deref().filter(|m| *m != SIMPLE) {
            return Err(format!("--ldapBindMethod: only simple binds are taken, got `{m}`"));
        }
        let default_port = if tls.is_some() { LDAPS_PORT } else { LDAP_PORT };
        let hosts = servers
            .split(',')
            .map(|h| host_port(h.trim(), default_port))
            .collect::<Result<_, _>>()?;
        let mapping = match &o.mapping {
            Some(m) => Mapping::from_config(m).map_err(crate::error_text)?,
            None => Mapping::default(),
        };
        let query = match (&o.query_user, &o.query_password) {
            (Some(u), Some(p)) => Some((u.clone(), p.0.as_bytes().to_vec())),
            (None, None) => None,
            _ => return Err("--ldapQueryUser and --ldapQueryPassword go together".into()),
        };
        let timeout = Duration::from_millis(o.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let authz = match &o.authz_query_template {
            Some(t) => Some(AuthzTemplate::parse(t).map_err(crate::error_text)?),
            None => None,
        };
        let cache_interval =
            Duration::from_secs(o.cache_interval_secs.unwrap_or(DEFAULT_CACHE_INTERVAL_SECS));
        Ok(Some(LdapServer {
            hosts,
            tls,
            query,
            mapping,
            timeout,
            authz,
            cache: Mutex::new(HashMap::new()),
            cache_interval,
        }))
    }

    /// Forgets the groups read of every user (`invalidateUserCache`).
    pub fn invalidate(&self) {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// The groups of `user` read less than `ldapUserCacheInvalidationInterval` ago.
    fn cached(&self, user: &str) -> Option<Vec<String>> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.get(user).filter(|(at, _)| at.elapsed() < self.cache_interval).map(|(_, g)| g.clone())
    }

    /// Whether `user` binds with `password` and, with `--ldapAuthzQueryTemplate`, its roles: the
    /// DNs of its groups, roles of `admin` (013 FR-026), kept for
    /// `ldapUserCacheInvalidationInterval` seconds. A refused bind or a user without an entry is
    /// [`LdapLogin::Refused`]; a server that does not answer, an error.
    pub fn login(&self, user: &str, password: &[u8]) -> Result<LdapLogin, String> {
        let mut last = String::from("no LDAP server");
        for (host, port) in &self.hosts {
            match self.login_at(host, *port, user, password) {
                Ok(done) => return Ok(done),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn login_at(
        &self,
        host: &str,
        port: u16,
        user: &str,
        password: &[u8],
    ) -> Result<LdapLogin, String> {
        let query = self.query.as_ref().map(|(u, p)| (u.as_str(), p.as_slice()));
        let (mut login, first) = match Login::start(&self.mapping, user, password, query) {
            Ok(l) => l,
            Err(LdapError::NoUser | LdapError::Refused(_)) => return Ok(LdapLogin::Refused),
            Err(e) => return Err(e.to_string()),
        };
        let cached = self.cached(user);
        if let (Some(t), None) = (&self.authz, &cached) {
            login = login.with_groups(t, user);
        }
        let mut link = self.connect(host, port)?;
        link.send(&first)?;
        loop {
            match login.receive(&link.recv()?) {
                Ok(Step::Send(bytes)) => link.send(&bytes)?,
                Ok(Step::Read) => {}
                Ok(Step::Done { unbind, groups, .. }) => {
                    let _ = link.send(&unbind);
                    if self.authz.is_none() {
                        return Ok(LdapLogin::LoggedIn);
                    }
                    let groups = match (groups, cached) {
                        (Some(g), _) => {
                            let mut cache =
                                self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                            cache.insert(user.to_owned(), (Instant::now(), g.clone()));
                            g
                        }
                        (None, c) => c.unwrap_or_default(),
                    };
                    let roles = groups.into_iter().map(|g| RoleName::new(g, ADMIN_DB)).collect();
                    return Ok(LdapLogin::Authorized(roles));
                }
                Err(LdapError::NoUser | LdapError::Refused(_)) => return Ok(LdapLogin::Refused),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

impl LdapServer {
    /// A connection to `host`: each address of the name in turn (`localhost` is `::1` and
    /// `127.0.0.1`).
    fn connect(&self, host: &str, port: u16) -> Result<Link, String> {
        let addrs = (host, port).to_socket_addrs().map_err(|e| format!("{host}:{port}: {e}"))?;
        let mut last = format!("{host}:{port}: no address");
        let mut connected = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.timeout) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        let sock = connected.ok_or(last)?;
        sock.set_read_timeout(Some(self.timeout)).map_err(crate::error_text)?;
        sock.set_write_timeout(Some(self.timeout)).map_err(crate::error_text)?;
        Link::new(sock, self.tls.clone(), host)
    }

    /// With `--ldapAuthzQueryTemplate`, the roles of `user`, who logged in another way
    /// (REFDB-X509, 013 FR-033): the DNs of its groups, roles of `admin`, kept like those of
    /// a PLAIN login; `None` without the template. A name the mapping does not take has no
    /// groups.
    pub fn groups(&self, user: &str) -> Result<Option<Vec<RoleName>>, String> {
        let Some(template) = &self.authz else { return Ok(None) };
        let as_roles =
            |g: Vec<String>| Some(g.into_iter().map(|g| RoleName::new(g, ADMIN_DB)).collect());
        if let Some(g) = self.cached(user) {
            return Ok(as_roles(g));
        }
        let query = self.query.as_ref().map(|(u, p)| (u.as_str(), p.as_slice()));
        let (mut login, first) = match Login::groups_of(&self.mapping, user, query, template) {
            Ok(l) => l,
            Err(LdapError::NoUser) => return Ok(Some(Vec::new())),
            Err(e) => return Err(e.to_string()),
        };
        let mut last = String::from("no LDAP server");
        for (host, port) in &self.hosts {
            let mut link = match self.connect(host, *port) {
                Ok(l) => l,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            link.send(&first)?;
            loop {
                match login.receive(&link.recv()?) {
                    Ok(Step::Send(bytes)) => link.send(&bytes)?,
                    Ok(Step::Read) => {}
                    Ok(Step::Done { unbind, groups, .. }) => {
                        let _ = link.send(&unbind);
                        let g = groups.unwrap_or_default();
                        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                        cache.insert(user.to_owned(), (Instant::now(), g.clone()));
                        return Ok(as_roles(g));
                    }
                    Err(LdapError::NoUser) => return Ok(Some(Vec::new())),
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        Err(last)
    }
}

/// A connection to the LDAP server, with TLS or without.
enum Link {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
}

impl Link {
    fn new(sock: TcpStream, tls: Option<Arc<ClientConfig>>, host: &str) -> Result<Link, String> {
        Ok(match tls {
            None => Link::Plain(sock),
            Some(cfg) => Link::Tls(Box::new(TlsStream::client(sock, cfg, host)?)),
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self {
            Link::Plain(s) => s.write_all(bytes).map_err(crate::error_text),
            Link::Tls(t) => t.send(bytes),
        }
    }

    /// Bytes from the server (at least one).
    fn recv(&mut self) -> Result<Vec<u8>, String> {
        let data = match self {
            Link::Plain(s) => {
                let mut buf = vec![0u8; 16 << 10];
                let n = s.read(&mut buf).map_err(crate::error_text)?;
                buf.truncate(n);
                buf
            }
            Link::Tls(t) => t.recv()?,
        };
        if data.is_empty() {
            return Err("the LDAP server closed the connection".into());
        }
        Ok(data)
    }
}

/// The node's LDAP server, shared by its workers; two are equal when they are the same one.
#[derive(Clone, Debug)]
pub struct SharedLdap(pub Arc<LdapServer>);

impl PartialEq for SharedLdap {
    fn eq(&self, other: &SharedLdap) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedLdap {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use teseladb_auth::ldap_ber::{self, tag};
    use teseladb_tls::{CertifiedKey, ServerConfig, Trust};
    use teseladb_x509::ext::GeneralName;

    /// An LDAP server: `uid=<u>,ou=people` binds with `<u>-pw`; a search whose request holds
    /// `uid=jane` finds `cn=readers`. Over TLS with `tls`. Counts the searches.
    fn server(tls: Option<Arc<ServerConfig>>, searches: Arc<AtomicUsize>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(sock) = sock else { return };
                let (tls, searches) = (tls.clone(), searches.clone());
                std::thread::spawn(move || {
                    let mut link = match tls {
                        None => Link::Plain(sock),
                        Some(c) => match TlsStream::server(sock, c, &[]) {
                            Ok(s) => Link::Tls(Box::new(s)),
                            Err(_) => return,
                        },
                    };
                    let mut buf = Vec::new();
                    while let Ok(data) = link.recv() {
                        buf.extend_from_slice(&data);
                        while let Ok(Some((_, content, rest))) = ldap_ber::read(&buf) {
                            let used = buf.len() - rest.len();
                            let (id, op) = ldap_ber::expect(content, tag::INTEGER).unwrap();
                            let (t, body, _) = ldap_ber::read_whole(op).unwrap();
                            let answer = |op: Vec<u8>| {
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
                                let user = dn
                                    .strip_prefix("uid=")
                                    .and_then(|d| d.strip_suffix(",ou=people"));
                                let ok = user
                                    .is_some_and(|u| parts[2].1 == format!("{u}-pw").as_bytes());
                                let _ = link.send(&answer(ldap_ber::element(
                                    0x61,
                                    &result(if ok { 0 } else { 49 }),
                                )));
                            }
                            if t == 0x63 {
                                searches.fetch_add(1, Ordering::SeqCst);
                                if body.windows(8).any(|w| w == b"uid=jane") {
                                    let mut e = ldap_ber::element(tag::OCTET_STRING, b"cn=readers");
                                    ldap_ber::put(&mut e, tag::SEQUENCE, &[]);
                                    let _ = link.send(&answer(ldap_ber::element(0x64, &e)));
                                }
                                let _ = link.send(&answer(ldap_ber::element(0x65, &result(0))));
                            }
                            buf.drain(..used);
                        }
                    }
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

    #[test]
    fn plain_logins_and_groups_with_their_cache() {
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
        // Kept for the interval, then searched again; and forgotten on invalidation.
        assert_eq!(s.login("jane", b"jane-pw"), Ok(roles(&["cn=readers"])));
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

    /// The groups of a REFDB-X509 user (013 T045): only with the authorization template,
    /// without a bind of the user, kept like those of a PLAIN login; a dead server is an error.
    #[test]
    fn groups_of_a_user_that_logged_in_another_way() {
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
        // A name the mapping does not take has no groups.
        let narrow = LdapOpts {
            mapping: Some("[{match: 'uid=(.+)', substitution: 'uid={0},ou=people'}]".into()),
            ..o.clone()
        };
        let s = LdapServer::from_opts(&narrow).unwrap().unwrap();
        assert_eq!(s.groups("jane"), Ok(Some(Vec::new())));
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let s =
            LdapServer::from_opts(&LdapOpts { servers: Some(format!("127.0.0.1:{dead}")), ..o })
                .unwrap()
                .unwrap();
        assert!(s.groups("jane").is_err());
    }

    #[test]
    fn each_server_is_tried_and_errors_are_named() {
        let port = server(None, Arc::new(AtomicUsize::new(0)));
        // A dead server first: the next one answers.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let o =
            LdapOpts { servers: Some(format!("127.0.0.1:{dead},127.0.0.1:{port}")), ..opts(port) };
        let s = LdapServer::from_opts(&o).unwrap().unwrap();
        assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::LoggedIn));
        let o = LdapOpts { servers: Some(format!("127.0.0.1:{dead}")), ..opts(port) };
        let s = LdapServer::from_opts(&o).unwrap().unwrap();
        assert!(s.login("jane", b"jane-pw").is_err());
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
        assert!(s.login("jane", b"jane-pw").is_err());
        // A mapping with a search needs the query user's bind first; a user no rule maps.
        let o = LdapOpts {
            mapping: Some("[{match: 'x(.+)', ldapQuery: 'dc=x??sub?(uid={0})'}]".into()),
            ..opts(port)
        };
        let s = LdapServer::from_opts(&o).unwrap().unwrap();
        assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::Refused));
    }

    #[test]
    fn ldaps_with_the_servers_certificate() {
        let dir = std::env::temp_dir().join(format!("teseladb-ldaps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap());
        let params =
            crate::pki::PkiParams { now, node_names: vec![GeneralName::Dns("localhost".into())] };
        let pki = crate::pki::load_or_create(&dir, &params, &mut |b| teseladb_sys::fill_random(b))
            .unwrap();
        let identity = CertifiedKey { chain: pki.node_chain.clone(), key: pki.node_key.clone() };
        let port =
            server(Some(Arc::new(ServerConfig::new(identity))), Arc::new(AtomicUsize::new(0)));
        let mut s = LdapServer::from_opts(&opts(port)).unwrap().unwrap();
        s.hosts = vec![("localhost".into(), port)];
        s.tls = Some(Arc::new(ClientConfig::new(Trust::anchors(vec![pki.ca.clone()]))));
        assert_eq!(s.login("jane", b"jane-pw"), Ok(LdapLogin::LoggedIn));
        // A server whose certificate the client does not trust.
        s.tls = Some(Arc::new(ClientConfig::new(Trust::anchors(Vec::new()))));
        assert!(s.login("jane", b"jane-pw").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_options_are_refused() {
        let bad = |o: LdapOpts| LdapServer::from_opts(&o).unwrap_err();
        assert!(LdapServer::from_opts(&LdapOpts::default()).unwrap().is_none());
        let none = LdapOpts { servers: None, ..opts(1) };
        assert!(bad(none).contains("need --ldapServers"));
        // Any one LDAP option without servers is refused.
        for o in [
            LdapOpts { mapping: Some("[]".into()), ..LdapOpts::default() },
            LdapOpts { transport: Some("none".into()), ..LdapOpts::default() },
            LdapOpts { authz_query_template: Some("dc=x".into()), ..LdapOpts::default() },
            LdapOpts { query_user: Some("cn=q".into()), ..LdapOpts::default() },
        ] {
            assert!(bad(o).contains("need --ldapServers"));
        }
        assert!(bad(LdapOpts { transport: Some("ssl".into()), ..opts(1) }).contains("tls or none"));
        assert!(bad(LdapOpts { bind_method: Some("sasl".into()), ..opts(1) }).contains("simple"));
        assert!(
            bad(LdapOpts { query_user: Some("cn=q".into()), ..opts(1) }).contains("go together")
        );
        assert!(bad(LdapOpts { mapping: Some("{".into()), ..opts(1) }).contains("userToDNMapping"));
        let t = Some("a?b?c?d?e".into());
        assert!(bad(LdapOpts { authz_query_template: t, ..opts(1) }).contains("queryTemplate"));
        for s in ["", "[::1", "[::1]x", "h:port", ":5"] {
            let o = LdapOpts { servers: Some(s.into()), ..opts(1) };
            assert!(bad(o).contains("bad server"), "{s}");
        }
        assert_eq!(host_port("[::1]:7", 1), Ok(("::1".into(), 7)));
        assert_eq!(host_port("[::1]", 1), Ok(("::1".into(), 1)));
        assert_eq!(host_port("h", 389), Ok(("h".into(), 389)));
        // LDAPS by default, with the system's authorities, on port 636.
        let o = LdapOpts { transport: None, servers: Some("h".into()), ..opts(1) };
        let s = LdapServer::from_opts(&o).unwrap().unwrap();
        assert_eq!((s.hosts[0].1, s.tls.is_some()), (LDAPS_PORT, true));
        assert!(s.tls.as_ref().is_some_and(|t| t.suites.iter().any(|s| !s.is_fips_approved())));
        assert!(s.tls.as_ref().is_some_and(|t| t.min_version == teseladb_tls::Version::Tls12));
        let fips = LdapServer::from_opts(&LdapOpts { fips: true, ..o }).unwrap().unwrap();
        assert!(fips.tls.is_some_and(|t| t.suites.iter().all(|s| s.is_fips_approved())));
        let o = LdapOpts {
            query_user: Some("cn=q".into()),
            query_password: Some(Password::from("q")),
            ..opts(1)
        };
        assert!(LdapServer::from_opts(&o).unwrap().unwrap().query.is_some());
        // A system without authorities stops LDAPS.
        let o = LdapOpts { transport: None, servers: Some("h".into()), ..opts(1) };
        let none = || Err("no bundle".to_owned());
        assert_eq!(
            LdapServer::from_opts_with(&o, none).unwrap_err(),
            "--ldapTransportSecurity tls: no bundle"
        );
    }

    #[test]
    fn a_mapping_with_a_search_binds_the_query_user_first() {
        let searches = Arc::new(AtomicUsize::new(0));
        let port = server(None, searches.clone());
        // The query user binds, the search maps `jane` to `cn=readers`, whose bind with
        // `jane-pw` the server refuses.
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
}
