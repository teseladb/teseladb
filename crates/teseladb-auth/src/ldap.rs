//! Authentication against LDAP (013 FR-025; RFC 4511): the messages a client sends and reads,
//! RefDb's `security.ldap.userToDNMapping` (each rule a regular expression on the user name
//! with a DN `substitution` or an `ldapQuery` whose capture groups fill `{0}`, `{1}`...), and the
//! conversation of a PLAIN login without I/O: optionally bind as the query user and search the
//! user's DN, then bind as the user with the password. The connection, with TLS, is the server's.

use std::fmt;

use teseladb_mql::regex::Regex;

use crate::ldap_ber::{self, BerError, tag};
use crate::ldap_filter::{self, Query};

/// LDAP's version.
const VERSION: i64 = 3;
/// `resultCode` of success.
pub const SUCCESS: i64 = 0;
/// `resultCode` of a wrong password (`invalidCredentials`).
pub const INVALID_CREDENTIALS: i64 = 49;

const BIND_REQUEST: u8 = 0x60;
const BIND_RESPONSE: u8 = 0x61;
const UNBIND_REQUEST: u8 = 0x42;
const SEARCH_REQUEST: u8 = 0x63;
const SEARCH_ENTRY: u8 = 0x64;
const SEARCH_DONE: u8 = 0x65;
const SEARCH_REFERENCE: u8 = 0x73;
const SIMPLE: u8 = 0x80;

/// The result of an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LdapResult {
    /// `resultCode`.
    pub code: i64,
    /// `matchedDN`.
    pub matched_dn: String,
    /// `diagnosticMessage`.
    pub message: String,
}

/// An operation of a server's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `BindResponse`.
    BindResponse(LdapResult),
    /// `SearchResultEntry`: the DN and each attribute with its values.
    SearchEntry {
        /// Its DN.
        dn: String,
        /// Attributes.
        attributes: Vec<(String, Vec<Vec<u8>>)>,
    },
    /// `SearchResultReference`.
    SearchReference,
    /// `SearchResultDone`.
    SearchDone(LdapResult),
    /// Any other operation, by tag.
    Other(u8),
}

/// A server's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// `messageID`.
    pub id: i64,
    /// The operation.
    pub op: Op,
}

fn message(id: i64, op: &[u8]) -> Vec<u8> {
    let mut c = ldap_ber::element(tag::INTEGER, &ldap_ber::int_content(id));
    c.extend_from_slice(op);
    ldap_ber::element(tag::SEQUENCE, &c)
}

/// A simple `BindRequest` of `dn` with `password`.
pub fn bind_request(id: i64, dn: &str, password: &[u8]) -> Vec<u8> {
    let mut c = ldap_ber::element(tag::INTEGER, &ldap_ber::int_content(VERSION));
    ldap_ber::put(&mut c, tag::OCTET_STRING, dn.as_bytes());
    ldap_ber::put(&mut c, SIMPLE, password);
    message(id, &ldap_ber::element(BIND_REQUEST, &c))
}

/// An `UnbindRequest`.
pub fn unbind_request(id: i64) -> Vec<u8> {
    message(id, &ldap_ber::element(UNBIND_REQUEST, &[]))
}

/// A `SearchRequest` of `query`, its filter already filled.
pub fn search_request(id: i64, query: &Query) -> Result<Vec<u8>, BerError> {
    let mut c = ldap_ber::element(tag::OCTET_STRING, query.base.as_bytes());
    ldap_ber::put(&mut c, tag::ENUMERATED, &ldap_ber::int_content(query.scope.code()));
    // derefAliases neverDerefAliases, no size or time limit, types and values.
    ldap_ber::put(&mut c, tag::ENUMERATED, &[0]);
    ldap_ber::put(&mut c, tag::INTEGER, &[0]);
    ldap_ber::put(&mut c, tag::INTEGER, &[0]);
    ldap_ber::put(&mut c, tag::BOOLEAN, &[0]);
    c.extend_from_slice(&ldap_filter::encode(&query.filter)?);
    let mut attrs = Vec::new();
    for a in &query.attributes {
        ldap_ber::put(&mut attrs, tag::OCTET_STRING, a.as_bytes());
    }
    ldap_ber::put(&mut c, tag::SEQUENCE, &attrs);
    Ok(message(id, &ldap_ber::element(SEARCH_REQUEST, &c)))
}

fn text(bytes: &[u8]) -> Result<String, BerError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| BerError("not UTF-8"))
}

fn result(content: &[u8]) -> Result<LdapResult, BerError> {
    let (code, rest) = ldap_ber::expect(content, tag::ENUMERATED)?;
    let (dn, rest) = ldap_ber::expect(rest, tag::OCTET_STRING)?;
    let (msg, _) = ldap_ber::expect(rest, tag::OCTET_STRING)?;
    Ok(LdapResult { code: ldap_ber::int_value(code)?, matched_dn: text(dn)?, message: text(msg)? })
}

fn entry(content: &[u8]) -> Result<Op, BerError> {
    let (dn, rest) = ldap_ber::expect(content, tag::OCTET_STRING)?;
    let (list, _) = ldap_ber::expect(rest, tag::SEQUENCE)?;
    let mut attributes = Vec::new();
    for (t, attr) in ldap_ber::elements(list)? {
        if t != tag::SEQUENCE {
            return Err(BerError("attribute"));
        }
        let (name, rest) = ldap_ber::expect(attr, tag::OCTET_STRING)?;
        let (vals, _) = ldap_ber::expect(rest, tag::SET)?;
        let values = ldap_ber::elements(vals)?.into_iter().map(|(_, v)| v.to_vec()).collect();
        attributes.push((text(name)?, values));
    }
    Ok(Op::SearchEntry { dn: text(dn)?, attributes })
}

/// Reads the message at the start of `input`: it and the bytes it took, or `Ok(None)` while
/// it is not whole.
pub fn read_message(input: &[u8]) -> Result<Option<(Message, usize)>, BerError> {
    let Some((t, content, rest)) = ldap_ber::read(input)? else { return Ok(None) };
    if t != tag::SEQUENCE {
        return Err(BerError("not an LDAPMessage"));
    }
    let (id, rest_ops) = ldap_ber::expect(content, tag::INTEGER)?;
    let (op_tag, op, _) = ldap_ber::read_whole(rest_ops)?;
    let op = match op_tag {
        BIND_RESPONSE => Op::BindResponse(result(op)?),
        SEARCH_ENTRY => entry(op)?,
        SEARCH_DONE => Op::SearchDone(result(op)?),
        SEARCH_REFERENCE => Op::SearchReference,
        other => Op::Other(other),
    };
    Ok(Some((Message { id: ldap_ber::int_value(id)?, op }, input.len() - rest.len())))
}

/// Why a login failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LdapError {
    /// The configuration cannot be used.
    Config(String),
    /// The user maps to no DN, or its query found none or several.
    NoUser,
    /// The server refused a bind with this result.
    Refused(LdapResult),
    /// The server's answer is not LDAP, or not the one expected.
    Protocol(String),
}

impl fmt::Display for LdapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LdapError::Config(m) => write!(f, "LDAP configuration: {m}"),
            LdapError::NoUser => f.write_str("the user has no LDAP entry"),
            LdapError::Refused(r) => {
                write!(f, "LDAP bind failed with code {}: {}", r.code, r.message)
            }
            LdapError::Protocol(m) => write!(f, "LDAP protocol: {m}"),
        }
    }
}

impl std::error::Error for LdapError {}

impl From<BerError> for LdapError {
    fn from(e: BerError) -> LdapError {
        LdapError::Protocol(e.to_string())
    }
}

/// What a mapping rule turns the user name into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A DN.
    Dn(String),
    /// A search whose single result is the DN.
    Query(Query),
}

/// One rule of `userToDNMapping`.
#[derive(Debug)]
pub struct Rule {
    regex: Regex,
    /// `substitution` (a DN) or `ldapQuery`, with `{n}` for the capture groups.
    template: String,
    query: bool,
}

/// `security.ldap.userToDNMapping`.
#[derive(Debug, Default)]
pub struct Mapping {
    rules: Vec<Rule>,
}

impl Mapping {
    /// Adds a rule: `match` and either a `substitution` or an `ldapQuery`.
    pub fn push(&mut self, pattern: &str, template: &str, query: bool) -> Result<(), LdapError> {
        let regex = Regex::new(&format!(r"\A(?:{pattern})\z"), "")
            .map_err(|e| LdapError::Config(e.message))?;
        if query {
            // Checked with each `{n}` filled, as a user name fills it.
            let sample =
                (0..10).fold(template.to_owned(), |t, i| t.replace(&format!("{{{i}}}"), "x"));
            Query::parse(&sample).map_err(|e| LdapError::Config(e.to_string()))?;
        }
        self.rules.push(Rule { regex, template: template.to_owned(), query });
        Ok(())
    }

    /// `security.ldap.userToDNMapping`: an array of `{match, substitution}` and
    /// `{match, ldapQuery}` documents, in JSON as the shell writes it.
    pub fn from_config(text: &str) -> Result<Mapping, LdapError> {
        use teseladb_bson::Value;
        let config = |m: &str| LdapError::Config(format!("userToDNMapping: {m}"));
        let wrapped = format!("{{\"m\":{}}}", crate::shell_json::quote_keys(text));
        let doc = teseladb_bson::extjson::parse(&wrapped).map_err(|e| config(&e.to_string()))?;
        let Some(Value::Array(rules)) = doc.get("m") else { return Err(config("not an array")) };
        let mut mapping = Mapping::default();
        for rule in rules {
            let Value::Document(d) = rule else { return Err(config("a rule is not a document")) };
            let text = |k: &str| match d.get(k) {
                Some(Value::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let pattern = text("match").ok_or_else(|| config("a rule without match"))?;
            match (text("substitution"), text("ldapQuery")) {
                (Some(t), None) => mapping.push(pattern, t, false)?,
                (None, Some(q)) => mapping.push(pattern, q, true)?,
                _ => return Err(config("a rule needs either substitution or ldapQuery")),
            }
        }
        Ok(mapping)
    }

    /// The DN or the search of `user`: the first rule that matches it whole, with its groups;
    /// the name itself without rules; `None` when no rule matches.
    pub fn map(&self, user: &str) -> Result<Option<Target>, LdapError> {
        if self.rules.is_empty() {
            return Ok(Some(Target::Dn(user.to_owned())));
        }
        for rule in &self.rules {
            let found = rule.regex.find_at(user, 0).map_err(|e| LdapError::Config(e.message))?;
            let Some(m) = found else { continue };
            let groups: Vec<&str> =
                m.groups.iter().skip(1).map(|g| g.map_or("", |(a, b)| &user[a..b])).collect();
            let mut filled = rule.template.clone();
            for (i, g) in groups.iter().enumerate() {
                let value = if rule.query { ldap_filter::escape_value(g) } else { (*g).to_owned() };
                filled = filled.replace(&format!("{{{i}}}"), &value);
            }
            return Ok(Some(if rule.query {
                Target::Query(Query::parse(&filled).map_err(|e| LdapError::Config(e.to_string()))?)
            } else {
                Target::Dn(filled)
            }));
        }
        Ok(None)
    }
}

/// `security.ldap.authz.queryTemplate` (013 FR-026): the search of a user's groups, with
/// `{USER}` for the DN it logged in as and `{PROVIDED_USER}` for the name it gave. Without
/// attributes the DNs of the entries found are the groups; with attributes, their values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthzTemplate(Query);

/// `{USER}` in the template.
pub const USER_TOKEN: &str = "{USER}";
/// `{PROVIDED_USER}` in the template.
pub const PROVIDED_USER_TOKEN: &str = "{PROVIDED_USER}";

/// `value` as an attribute value inside a DN (RFC 4514, section 2.4).
pub fn escape_dn_value(value: &str) -> String {
    let last = value.chars().count().saturating_sub(1);
    let mut out = String::with_capacity(value.len());
    for (i, c) in value.chars().enumerate() {
        let special = matches!(c, ',' | '+' | '"' | '\\' | '<' | '>' | ';' | '=')
            || (i == 0 && matches!(c, ' ' | '#'))
            || (i == last && c == ' ');
        if c == '\0' {
            out.push_str("\\00");
            continue;
        }
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl AuthzTemplate {
    /// Reads the template; an error names what is wrong.
    pub fn parse(text: &str) -> Result<AuthzTemplate, LdapError> {
        let config = |e: BerError| LdapError::Config(format!("authz.queryTemplate: {e}"));
        let query = Query::parse(text).map_err(config)?;
        Ok(AuthzTemplate(query))
    }

    /// The search of the groups of the user whose DN is `dn` and who gave the name `provided`.
    pub fn query(&self, dn: &str, provided: &str) -> Query {
        let fill = |text: &str, user: String, given: String| {
            text.replace(USER_TOKEN, &user).replace(PROVIDED_USER_TOKEN, &given)
        };
        let q = &self.0;
        Query {
            base: fill(&q.base, dn.to_owned(), escape_dn_value(provided)),
            attributes: q.attributes.clone(),
            scope: q.scope,
            filter: fill(
                &q.filter,
                ldap_filter::escape_value(dn),
                ldap_filter::escape_value(provided),
            ),
        }
    }
}

/// What the conversation does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Write these bytes and read the answer.
    Send(Vec<u8>),
    /// The answer is not whole yet: read more.
    Read,
    /// The user's DN logged in; write the unbind and close.
    Done {
        /// The DN.
        dn: String,
        /// The unbind.
        unbind: Vec<u8>,
        /// The user's groups, with an authorization template.
        groups: Option<Vec<String>>,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    QueryBind(Query),
    Search { found: Vec<String> },
    UserBind(String),
    GroupsBind { dn: String, query: Query },
    Groups { dn: String, attributes: Vec<String>, found: Vec<String> },
    Finished,
}

/// The conversation of a PLAIN login.
#[derive(Debug)]
pub struct Login {
    password: Vec<u8>,
    query_user: Option<(String, Vec<u8>)>,
    authz: Option<(AuthzTemplate, String)>,
    next_id: i64,
    buf: Vec<u8>,
    phase: Phase,
    /// Only the groups of a user that logged in another way: the user never binds.
    groups_only: bool,
}

impl Drop for Login {
    fn drop(&mut self) {
        self.password.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.password);
        if let Some((_, p)) = self.query_user.as_mut() {
            p.iter_mut().for_each(|b| *b = 0);
            std::hint::black_box(&p);
        }
    }
}

impl Login {
    /// Starts the login of `user` with `password`: the first request to send. The query user
    /// (`bind.queryUser`, `bind.queryPassword`) binds first when the mapping gives a search.
    pub fn start(
        mapping: &Mapping,
        user: &str,
        password: &[u8],
        query_user: Option<(&str, &[u8])>,
    ) -> Result<(Login, Vec<u8>), LdapError> {
        // An empty password is an unauthenticated bind, which LDAP servers accept for anyone.
        if password.is_empty() {
            return Err(LdapError::Refused(LdapResult {
                code: INVALID_CREDENTIALS,
                matched_dn: String::new(),
                message: "empty password".into(),
            }));
        }
        let target = mapping.map(user)?.ok_or(LdapError::NoUser)?;
        let mut login = Login {
            password: password.to_vec(),
            query_user: query_user.map(|(u, p)| (u.to_owned(), p.to_vec())),
            authz: None,
            next_id: 1,
            buf: Vec::new(),
            phase: Phase::Finished,
            groups_only: false,
        };
        let first = match (target, query_user) {
            (Target::Dn(dn), _) => login.bind_user(dn),
            (Target::Query(q), Some((qu, qp))) => {
                let req = bind_request(login.id(), qu, qp);
                login.phase = Phase::QueryBind(q);
                req
            }
            (Target::Query(q), None) => login.search(q)?,
        };
        Ok((login, first))
    }

    /// Starts the search of the groups of `user`, who logged in another way (REFDB-X509,
    /// 013 FR-033): its DN from the mapping, then its groups with `template`, as the query user
    /// when there is one; the user never binds.
    pub fn groups_of(
        mapping: &Mapping,
        user: &str,
        query_user: Option<(&str, &[u8])>,
        template: &AuthzTemplate,
    ) -> Result<(Login, Vec<u8>), LdapError> {
        let target = mapping.map(user)?.ok_or(LdapError::NoUser)?;
        let mut login = Login {
            password: Vec::new(),
            query_user: query_user.map(|(u, p)| (u.to_owned(), p.to_vec())),
            authz: Some((template.clone(), user.to_owned())),
            next_id: 1,
            buf: Vec::new(),
            phase: Phase::Finished,
            groups_only: true,
        };
        let first = match (target, query_user) {
            (Target::Dn(dn), _) => login.groups_request(dn)?,
            (Target::Query(q), Some((qu, qp))) => {
                let req = bind_request(login.id(), qu, qp);
                login.phase = Phase::QueryBind(q);
                req
            }
            (Target::Query(q), None) => login.search(q)?,
        };
        Ok((login, first))
    }

    /// After the user's bind, searches its groups with `template` (013 FR-026), as the query
    /// user when there is one; `user` is the name the user gave.
    #[must_use]
    pub fn with_groups(mut self, template: &AuthzTemplate, user: &str) -> Login {
        self.authz = Some((template.clone(), user.to_owned()));
        self
    }

    fn id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn bind_user(&mut self, dn: String) -> Vec<u8> {
        let req = bind_request(self.id(), &dn, &self.password);
        self.phase = Phase::UserBind(dn);
        req
    }

    fn groups(&mut self, dn: String, query: Query) -> Result<Vec<u8>, LdapError> {
        let req = search_request(self.id(), &query)?;
        self.phase = Phase::Groups { dn, attributes: query.attributes, found: Vec::new() };
        Ok(req)
    }

    /// After the user's bind: its groups, or the end.
    fn after_bind(&mut self, dn: String) -> Result<Step, LdapError> {
        if self.authz.is_none() {
            let unbind = unbind_request(self.id());
            return Ok(Step::Done { dn, unbind, groups: None });
        }
        Ok(Step::Send(self.groups_request(dn)?))
    }

    /// The request that leads to the groups of `dn`: the search, or first the bind of the query
    /// user.
    fn groups_request(&mut self, dn: String) -> Result<Vec<u8>, LdapError> {
        let Some((template, provided)) = self.authz.take() else {
            return Err(LdapError::Protocol("no authorization template".into()));
        };
        let query = template.query(&dn, &provided);
        let Some((user, password)) = self.query_user.as_ref() else {
            return self.groups(dn, query);
        };
        let req = bind_request(self.next_id, user, password);
        self.next_id += 1;
        self.phase = Phase::GroupsBind { dn, query };
        Ok(req)
    }

    fn search(&mut self, q: Query) -> Result<Vec<u8>, LdapError> {
        let req = search_request(self.id(), &q)?;
        self.phase = Phase::Search { found: Vec::new() };
        Ok(req)
    }

    /// Takes bytes from the server; what to do next.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Step, LdapError> {
        self.buf.extend_from_slice(bytes);
        loop {
            let Some((msg, used)) = read_message(&self.buf)? else { return Ok(Step::Read) };
            self.buf.drain(..used);
            if msg.id != self.next_id - 1 {
                return Err(LdapError::Protocol("an answer to another request".into()));
            }
            match (std::mem::replace(&mut self.phase, Phase::Finished), msg.op) {
                (Phase::QueryBind(q), Op::BindResponse(r)) if r.code == SUCCESS => {
                    return Ok(Step::Send(self.search(q)?));
                }
                (
                    Phase::QueryBind(_) | Phase::UserBind(_) | Phase::GroupsBind { .. },
                    Op::BindResponse(r),
                ) if r.code != SUCCESS => {
                    return Err(LdapError::Refused(r));
                }
                (Phase::Search { mut found }, Op::SearchEntry { dn, .. }) => {
                    found.push(dn);
                    self.phase = Phase::Search { found };
                }
                (p @ Phase::Search { .. }, Op::SearchReference) => self.phase = p,
                (Phase::Search { mut found }, Op::SearchDone(r)) => {
                    if r.code != SUCCESS {
                        return Err(LdapError::Refused(r));
                    }
                    let (Some(dn), None) = (found.pop(), found.pop()) else {
                        return Err(LdapError::NoUser);
                    };
                    if self.groups_only {
                        return Ok(Step::Send(self.groups_request(dn)?));
                    }
                    return Ok(Step::Send(self.bind_user(dn)));
                }
                (Phase::UserBind(dn), Op::BindResponse(_)) => return self.after_bind(dn),
                (Phase::GroupsBind { dn, query }, Op::BindResponse(_)) => {
                    return Ok(Step::Send(self.groups(dn, query)?));
                }
                (
                    Phase::Groups { dn, attributes, mut found },
                    Op::SearchEntry { dn: entry, attributes: values },
                ) => {
                    if attributes.is_empty() {
                        found.push(entry);
                    }
                    let wanted = values.into_iter().filter(|(name, _)| {
                        attributes.iter().any(|a| a.eq_ignore_ascii_case(name))
                    });
                    for (_, vs) in wanted {
                        found.extend(vs.iter().map(|v| String::from_utf8_lossy(v).into_owned()));
                    }
                    self.phase = Phase::Groups { dn, attributes, found };
                }
                (p @ Phase::Groups { .. }, Op::SearchReference) => self.phase = p,
                (Phase::Groups { dn, found, .. }, Op::SearchDone(r)) => {
                    if r.code != SUCCESS {
                        return Err(LdapError::Refused(r));
                    }
                    let unbind = unbind_request(self.id());
                    return Ok(Step::Done { dn, unbind, groups: Some(found) });
                }
                (_, op) => return Err(LdapError::Protocol(format!("unexpected answer {op:?}"))),
            }
        }
    }
}
