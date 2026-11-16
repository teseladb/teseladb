//! The node's LDAP server (013 FR-025): the reference server's enterprise edition's options
//! `--ldapServers`, `--ldapTransportSecurity`, `--ldapBindMethod`, `--ldapQueryUser`,
//! `--ldapQueryPassword`, `--ldapUserToDNMapping` and `--ldapTimeoutMS`, and the connection a PLAIN
//! login opens to run the conversation of [`teseladb_auth::ldap::Login`]: plain TCP with `none`,
//! LDAPS (TLS from the first byte) with `tls`, trusting the system's certificate authorities. Each
//! server of the list is tried in turn until one answers.
//!
//! The connection is an engine one (023 FR-024): the name resolved off the shard, each step
//! within `--ldapTimeoutMS` on the engine's timers. A shard awaits [`LdapServer::login_on_shard`]
//! and [`LdapServer::groups_on_shard`]; a thread that is not a shard calls [`LdapServer::login`]
//! and [`LdapServer::groups`], which run them on the node's engine.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use teseladb_auth::ldap::{AuthzTemplate, LdapError, Login, Mapping};
#[cfg(target_os = "linux")]
use teseladb_auth::ldap::Step;
use teseladb_auth::roles::ADMIN_DB;
use teseladb_auth::users::RoleName;
use teseladb_tls::{ClientConfig, Trust};
use teseladb_wire::commands::LdapLogin;

use crate::cli::Password;

/// LDAP's port without TLS.
const LDAP_PORT: u16 = 389;
/// LDAPS's port.
const LDAPS_PORT: u16 = 636;
/// `--ldapTimeoutMS` by default, as the reference server.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const NONE: &str = "none";
const TLS: &str = "tls";
const SIMPLE: &str = "simple";
/// The error of a server that closed the connection before the conversation ended.
#[cfg(target_os = "linux")]
pub const CLOSED: &str = "the LDAP server closed the connection";
/// The error when the list of servers is empty.
#[cfg(target_os = "linux")]
const NO_SERVER: &str = "no LDAP server";

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

/// `ldapUserCacheInvalidationInterval` by default, as the reference server: 30 seconds.
pub const DEFAULT_CACHE_INTERVAL_SECS: u64 = 30;

/// What the server is, shared by the clones of an [`LdapServer`].
struct Inner {
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

/// The node's LDAP server, ready to log users in; its clones share it (and its cache).
#[derive(Clone)]
pub struct LdapServer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LdapServer {
    /// Without the query password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LdapServer")
            .field("hosts", &self.inner.hosts)
            .field("tls", &self.inner.tls.is_some())
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

/// How a conversation with one server ended: the groups it read (with a template), or the LDAP
/// error that ended it.
#[cfg(target_os = "linux")]
type Ended = Result<Option<Vec<String>>, LdapError>;

/// How the groups' search of a user starts.
enum GroupsStart {
    /// The conversation and its first request.
    Ask(Login, Vec<u8>),
    /// The answer, without a server: no template, the cache's, or a name the mapping does not
    /// take.
    Answered(Option<Vec<RoleName>>),
}

impl LdapServer {
    /// The server the options describe; `None` without `--ldapServers`.
    pub fn from_opts(o: &LdapOpts) -> Result<Option<LdapServer>, String> {
        LdapServer::from_opts_with(o, crate::https::system_trust)
    }

    /// [`LdapServer::from_opts`] with the authorities of `--ldapTransportSecurity tls` from
    /// `system_trust` in place of the system's.
    pub fn from_opts_with(
        o: &LdapOpts,
        system_trust: impl FnOnce() -> Result<Trust, String>,
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
                // TLS 1.2 or 1.3, as the reference server reaches LDAP servers through OpenSSL.
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
        let inner = Inner {
            hosts,
            tls,
            query,
            mapping,
            timeout,
            authz,
            cache: Mutex::new(HashMap::new()),
            cache_interval,
        };
        Ok(Some(LdapServer { inner: Arc::new(inner) }))
    }

    /// The servers, in the order they are tried.
    pub fn hosts(&self) -> &[(String, u16)] {
        &self.inner.hosts
    }

    /// The client side of LDAPS; `None` in clear.
    pub fn tls(&self) -> Option<&Arc<ClientConfig>> {
        self.inner.tls.as_ref()
    }

    /// Whether the options gave a query user.
    pub fn has_query_user(&self) -> bool {
        self.inner.query.is_some()
    }

    /// Forgets the groups read of every user (`invalidateUserCache`).
    pub fn invalidate(&self) {
        self.inner.cache.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// The groups of `user` read less than `ldapUserCacheInvalidationInterval` ago.
    fn cached(&self, user: &str) -> Option<Vec<String>> {
        let cache = self.inner.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(user)
            .filter(|(at, _)| at.elapsed() < self.inner.cache_interval)
            .map(|(_, g)| g.clone())
    }

    #[cfg(target_os = "linux")]
    fn remember(&self, user: &str, groups: &[String]) {
        let mut cache = self.inner.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.insert(user.to_owned(), (Instant::now(), groups.to_vec()));
    }

    fn query(&self) -> Option<(&str, &[u8])> {
        self.inner.query.as_ref().map(|(u, p)| (u.as_str(), p.as_slice()))
    }

    /// The conversation of a PLAIN login of `user`, and the groups the cache already has.
    fn login_start(
        &self,
        user: &str,
        password: &[u8],
    ) -> Result<Option<(Login, Vec<u8>, Option<Vec<String>>)>, String> {
        let (mut login, first) = match Login::start(&self.inner.mapping, user, password, self.query())
        {
            Ok(l) => l,
            Err(LdapError::NoUser | LdapError::Refused(_)) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let cached = self.cached(user);
        if let (Some(t), None) = (&self.inner.authz, &cached) {
            login = login.with_groups(t, user);
        }
        Ok(Some((login, first, cached)))
    }

    /// What a PLAIN login that ended as `ended` gives, `cached` the groups it did not read.
    #[cfg(target_os = "linux")]
    fn login_end(
        &self,
        user: &str,
        ended: Ended,
        cached: Option<Vec<String>>,
    ) -> Result<LdapLogin, String> {
        let groups = match ended {
            Ok(g) => g,
            Err(LdapError::NoUser | LdapError::Refused(_)) => return Ok(LdapLogin::Refused),
            Err(e) => return Err(e.to_string()),
        };
        if self.inner.authz.is_none() {
            return Ok(LdapLogin::LoggedIn);
        }
        let groups = match (groups, cached) {
            (Some(g), _) => {
                self.remember(user, &g);
                g
            }
            (None, c) => c.unwrap_or_default(),
        };
        Ok(LdapLogin::Authorized(as_roles(groups)))
    }

    /// The groups' search of `user`, who logged in another way.
    fn groups_start(&self, user: &str) -> Result<GroupsStart, String> {
        let Some(template) = &self.inner.authz else { return Ok(GroupsStart::Answered(None)) };
        if let Some(g) = self.cached(user) {
            return Ok(GroupsStart::Answered(Some(as_roles(g))));
        }
        match Login::groups_of(&self.inner.mapping, user, self.query(), template) {
            Ok((login, first)) => Ok(GroupsStart::Ask(login, first)),
            Err(LdapError::NoUser) => Ok(GroupsStart::Answered(Some(Vec::new()))),
            Err(e) => Err(e.to_string()),
        }
    }

    /// What a groups' search that ended as `ended` gives.
    #[cfg(target_os = "linux")]
    fn groups_end(&self, user: &str, ended: Ended) -> Result<Option<Vec<RoleName>>, String> {
        match ended {
            Ok(groups) => {
                let g = groups.unwrap_or_default();
                self.remember(user, &g);
                Ok(Some(as_roles(g)))
            }
            Err(LdapError::NoUser) => Ok(Some(Vec::new())),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The DNs of groups as roles of `admin` (013 FR-026).
fn as_roles(groups: Vec<String>) -> Vec<RoleName> {
    groups.into_iter().map(|g| RoleName::new(g, ADMIN_DB)).collect()
}

#[cfg(target_os = "linux")]
mod on_engine {
    use teseladb::engine::core::future::Future;
    use teseladb_io::engine::connect::TlsClient;
    use teseladb_io::engine::link::Link;

    use super::{CLOSED, Ended, GroupsStart, LdapLogin, LdapServer, Login, NO_SERVER, RoleName, Step};
    use crate::outbound::{self, text};

    impl LdapServer {
        /// A connection to `host`: each address of the name in turn (`localhost` is `::1` and
        /// `127.0.0.1`), with LDAPS's handshake when the server takes TLS.
        async fn connect(&self, host: &str, port: u16) -> Result<Link, String> {
            let tls = self.inner.tls.as_ref().map(|config| TlsClient {
                config: config.clone(),
                server_name: Some(host.to_owned()),
                resume: None,
            });
            let link = Link::dial(host, port, tls, self.inner.timeout).await;
            link.map_err(|e| format!("{host}:{port}: {}", text(&e)))
        }

        /// Whether `user` binds with `password` and, with `--ldapAuthzQueryTemplate`, its roles:
        /// the DNs of its groups, roles of `admin` (013 FR-026), kept for
        /// `ldapUserCacheInvalidationInterval` seconds. A refused bind or a user without an entry
        /// is [`LdapLogin::Refused`]; a server that does not answer, an error. On the shard that
        /// awaits it.
        pub fn login_on_shard(&self, user: &str, password: &[u8]) -> Future<LdapLogin> {
            let (s, user, password) = (self.clone(), user.to_owned(), password.to_vec());
            outbound::future(async move { s.login_async(&user, &password).await })
        }

        async fn login_async(&self, user: &str, password: &[u8]) -> Result<LdapLogin, String> {
            let mut last = String::from(NO_SERVER);
            for (host, port) in &self.inner.hosts {
                match self.login_at(host, *port, user, password).await {
                    Ok(done) => return Ok(done),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }

        async fn login_at(
            &self,
            host: &str,
            port: u16,
            user: &str,
            password: &[u8],
        ) -> Result<LdapLogin, String> {
            let Some((mut login, first, cached)) = self.login_start(user, password)? else {
                return Ok(LdapLogin::Refused);
            };
            let mut link = self.connect(host, port).await?;
            let ended = converse(&mut link, &mut login, &first).await;
            link.close().await;
            self.login_end(user, ended?, cached)
        }

        /// With `--ldapAuthzQueryTemplate`, the roles of `user`, who logged in another way
        /// (X.509 authentication, 013 FR-033): the DNs of its groups, roles of `admin`, kept like
        /// those of a PLAIN login; `None` without the template. A name the mapping does not take
        /// has no groups. On the shard that awaits it.
        pub fn groups_on_shard(&self, user: &str) -> Future<Option<Vec<RoleName>>> {
            let (s, user) = (self.clone(), user.to_owned());
            outbound::future(async move { s.groups_async(&user).await })
        }

        async fn groups_async(&self, user: &str) -> Result<Option<Vec<RoleName>>, String> {
            let (mut login, first) = match self.groups_start(user)? {
                GroupsStart::Ask(login, first) => (login, first),
                GroupsStart::Answered(answer) => return Ok(answer),
            };
            let mut last = String::from(NO_SERVER);
            for (host, port) in &self.inner.hosts {
                let mut link = match self.connect(host, *port).await {
                    Ok(l) => l,
                    Err(e) => {
                        last = e;
                        continue;
                    }
                };
                let ended = converse(&mut link, &mut login, &first).await;
                link.close().await;
                return self.groups_end(user, ended?);
            }
            Err(last)
        }

        /// [`LdapServer::login_on_shard`] from a thread that is not a shard.
        pub fn login(&self, user: &str, password: &[u8]) -> Result<LdapLogin, String> {
            let (s, user, password) = (self.clone(), user.to_owned(), password.to_vec());
            outbound::call(move || s.login_on_shard(&user, &password))
        }

        /// [`LdapServer::groups_on_shard`] from a thread that is not a shard.
        pub fn groups(&self, user: &str) -> Result<Option<Vec<RoleName>>, String> {
            let (s, user) = (self.clone(), user.to_owned());
            outbound::call(move || s.groups_on_shard(&user))
        }
    }

    /// Runs `login` over `link`, `first` sent first, to its end: the unbind is sent when it is
    /// done. The outer error is the connection's.
    async fn converse(link: &mut Link, login: &mut Login, first: &[u8]) -> Result<Ended, String> {
        link.send(first).await.map_err(|e| text(&e))?;
        loop {
            let data = link.recv().await.map_err(|e| text(&e))?;
            if data.empty() {
                return Err(CLOSED.into());
            }
            match login.receive(data.as_slice()) {
                Ok(Step::Send(bytes)) => link.send(&bytes).await.map_err(|e| text(&e))?,
                Ok(Step::Read) => {}
                Ok(Step::Done { unbind, groups, .. }) => {
                    let _ = link.send(&unbind).await;
                    return Ok(Ok(groups));
                }
                Err(e) => return Ok(Err(e)),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl LdapServer {
    /// The LDAP client runs on the engine, which only Linux has (023 D3); a login the options
    /// refuse before any server is still answered.
    pub fn login(&self, user: &str, password: &[u8]) -> Result<LdapLogin, String> {
        match self.login_start(user, password)? {
            None => Ok(LdapLogin::Refused),
            Some(_) => Err(crate::LINUX_ONLY.into()),
        }
    }

    /// See [`LdapServer::login`].
    pub fn groups(&self, user: &str) -> Result<Option<Vec<RoleName>>, String> {
        match self.groups_start(user)? {
            GroupsStart::Answered(answer) => Ok(answer),
            GroupsStart::Ask(..) => Err(crate::LINUX_ONLY.into()),
        }
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

    fn opts(port: u16) -> LdapOpts {
        LdapOpts {
            servers: Some(format!("127.0.0.1:{port}")),
            transport: Some(NONE.into()),
            mapping: Some("[{match: '(.+)', substitution: 'uid={0},ou=people'}]".into()),
            ..LdapOpts::default()
        }
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
        assert_eq!((s.hosts()[0].1, s.tls().is_some()), (LDAPS_PORT, true));
        assert!(s.tls().is_some_and(|t| t.suites.iter().any(|s| !s.is_fips_approved())));
        assert!(s.tls().is_some_and(|t| t.min_version == teseladb_tls::Version::Tls12));
        let fips = LdapServer::from_opts(&LdapOpts { fips: true, ..o }).unwrap().unwrap();
        assert!(fips.tls().is_some_and(|t| t.suites.iter().all(|s| s.is_fips_approved())));
        let o = LdapOpts {
            query_user: Some("cn=q".into()),
            query_password: Some(Password::from("q")),
            ..opts(1)
        };
        assert!(LdapServer::from_opts(&o).unwrap().unwrap().has_query_user());
        // A system without authorities stops LDAPS.
        let o = LdapOpts { transport: None, servers: Some("h".into()), ..opts(1) };
        let none = || Err("no bundle".to_owned());
        assert_eq!(
            LdapServer::from_opts_with(&o, none).unwrap_err(),
            "--ldapTransportSecurity tls: no bundle"
        );
        assert!(format!("{s:?}").contains("\"h\""));
    }

    /// What is answered without a server: an empty password, a user no rule maps, and the groups
    /// without a template or of a name the mapping does not take.
    #[test]
    fn answers_without_a_server() {
        #[cfg(target_os = "linux")]
        crate::outbound::test_engine::install();
        let s =LdapServer::from_opts(&opts(1)).unwrap().unwrap();
        assert_eq!(s.login("jane", b""), Ok(LdapLogin::Refused));
        assert_eq!(s.groups("jane"), Ok(None));
        let o = LdapOpts {
            mapping: Some("[{match: 'uid=(.+)', substitution: 'uid={0},ou=people'}]".into()),
            authz_query_template: Some("ou=groups??one?(member={USER})".into()),
            ..opts(1)
        };
        let s = LdapServer::from_opts(&o).unwrap().unwrap();
        assert_eq!(s.groups("jane"), Ok(Some(Vec::new())));
        assert_eq!(s.login("jane", b"pw"), Ok(LdapLogin::Refused));
    }
}
