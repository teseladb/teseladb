//! Authentication commands (013 FR-008): `saslStart`, `saslContinue` and `authenticate`,
//! `speculativeAuthenticate` in `hello`, the commands a client may run before it
//! authenticates, and what a connection keeps of its authentication. Fields, types, order of
//! checks and messages are those of RefDb 9.0.4 (measured on a `refrouter` with `--keyFile` and a
//! `refnode` with TLS and client certificates). `logout`, deprecated since 5.0, answers as a
//! command that does not exist (DV-014).

use std::fmt;

use teseladb_auth::sasl::{
    self, CONVERSATION_ID, Conversation, SERVER_NONCE_BYTES, SaslError, SaslMechanism, Step,
};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, User, UserName};
use teseladb_auth::{aws, kerberos, oidc, restrictions};
use teseladb_bson::{Document, Value};
use teseladb_x509::pem::{base64_decode_strict, base64_encode};

use super::{Backend, LdapLogin};
use crate::concerns::canonical;
use crate::error::{
    AUTHENTICATION_FAILED, BAD_VALUE, CommandError, IDL_FAILED_TO_PARSE, MECHANISM_UNAVAILABLE,
    PROTOCOL_ERROR, SASL_PAYLOAD_NOT_BASE64, UNAUTHORIZED, USER_NOT_FOUND,
};
use crate::types::{truthy, type_name, wrong_type, wrong_types};

/// `saslStart`.
pub const SASL_START: &str = "saslStart";
/// `saslContinue`.
pub const SASL_CONTINUE: &str = "saslContinue";
/// `authenticate`.
pub const AUTHENTICATE: &str = "authenticate";
/// The `hello` field that carries the first step of authentication.
pub const SPECULATIVE_AUTHENTICATE: &str = "speculativeAuthenticate";

/// The commands of this module.
pub const NAMES: [&str; 3] = [SASL_START, SASL_CONTINUE, AUTHENTICATE];

/// The commands a client runs before it authenticates (measured: everything else answers
/// `Unauthorized`).
const OPEN: [&str; 9] = [
    crate::hello::HELLO,
    crate::hello::IS_MASTER,
    crate::hello::IS_MASTER_LOWER,
    crate::dispatch::PING,
    super::diagnostics::LIST_COMMANDS,
    super::diagnostics::CONNECTION_STATUS,
    SASL_START,
    SASL_CONTINUE,
    AUTHENTICATE,
];

/// The types `saslStart.autoAuthorize` takes, in RefDb's order.
const AUTO_AUTHORIZE_TYPES: [&str; 5] = ["long", "decimal", "double", "bool", "int"];

/// Whether `name` is an authentication command.
pub fn is_auth_command(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Whether a client that did not authenticate may run `name`.
pub fn allowed_unauthenticated(name: &str) -> bool {
    OPEN.contains(&name)
}

/// The error of command `name` run before authenticating.
pub fn requires_authentication(name: &str) -> CommandError {
    CommandError::new(UNAUTHORIZED, format!("Command {} requires authentication", canonical(name)))
}

/// Who authenticated on a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticated {
    /// The user.
    pub name: UserName,
    /// How.
    pub mechanism: SaslMechanism,
    /// When, in milliseconds of cluster time (the `loginTime` of the audit log's `logout`).
    pub login_ms: u64,
    /// Its roles when they come from its LDAP groups (013 FR-026); `None`: those of its document.
    pub roles: Option<Vec<RoleName>>,
    /// Its tenant (007 FR-025), which the connection's requests run in.
    pub tenant: u32,
}

impl Authenticated {
    /// The user's direct roles: those of its LDAP groups, or else those of its document.
    pub fn direct_roles<B: Backend + ?Sized>(
        &self,
        backend: &mut B,
    ) -> Result<Vec<RoleName>, CommandError> {
        match &self.roles {
            Some(r) => Ok(r.clone()),
            None => Ok(backend.user(&self.name)?.map(|u| u.roles).unwrap_or_default()),
        }
    }
}

/// What a connection keeps of its authentication.
#[derive(Default)]
pub struct AuthState {
    /// Commands other than [`allowed_unauthenticated`] need an authenticated user (a node
    /// started without `--insecure`).
    pub required: bool,
    /// The REFDB-X509 user name of the client certificate, when the client sent one.
    pub peer_user: Option<String>,
    /// The client's address (`host:port`), for `authenticationRestrictions`.
    pub client_addr: Option<String>,
    /// The node's address the client reached, for `authenticationRestrictions`.
    pub server_addr: Option<String>,
    /// The authenticated user. RefDb 9.0 keeps one per connection.
    pub user: Option<Authenticated>,
    /// The SASL conversation in progress, and whether its payloads go as Base64 strings.
    conversation: Option<(Conversation, bool)>,
    /// The REFDB-AWS conversation in progress, and whether its payloads go as Base64 strings.
    aws: Option<(teseladb_auth::aws::Server, bool)>,
    /// A GSSAPI conversation: the context, the user, whether the security layer was offered,
    /// and whether its payloads go as Base64 strings.
    gss: Option<(kerberos::Context, UserName, bool, bool)>,
    /// A REFDB-OIDC conversation waiting for the token, and whether its payloads go as Base64
    /// strings.
    oidc: Option<bool>,
    /// The user and mechanism of the authentication in progress, for the audit log.
    pub attempt: Option<(UserName, String)>,
    /// The connection's identifier in the audit log, once drawn.
    pub audit_uuid: Option<[u8; 16]>,
    /// `authenticationMechanisms`; empty, RefDb's default.
    pub mechanisms: Vec<SaslMechanism>,
    /// The URI of the protected resource the connection serves (`/mcp`): an OIDC access token
    /// must be issued for it (RFC 8707, 019 FR-027) instead of the provider's `audience`.
    pub oidc_resource: Option<String>,
    /// The client certificate is a member's: issued by the cluster's CA with the organization,
    /// unit and domain components of the node's certificate (013 FR-034).
    pub peer_member: bool,
    /// `clusterAuthMode` lets a member's certificate log in as `__system`; set before each
    /// authentication command from the node's parameter.
    pub members_by_x509: bool,
}

impl fmt::Debug for AuthState {
    /// Without the conversation, which holds the user's stored keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthState")
            .field("required", &self.required)
            .field("peer_user", &self.peer_user)
            .field("user", &self.user)
            .field("client_addr", &self.client_addr)
            .field("server_addr", &self.server_addr)
            .field("conversation", &self.conversation.is_some())
            .finish()
    }
}

impl AuthState {
    /// Whether the connection may run command `name` now.
    pub fn may_run(&self, name: &str) -> bool {
        !self.required || self.user.is_some() || allowed_unauthenticated(name)
    }

    /// Records that `name` authenticated with `mechanism`. A connection authenticated as another
    /// user fails, as RefDb 9.0 allows one user per connection (measured: the second user's
    /// SCRAM ends in `Authentication failed.` and the first stays).
    fn login(
        &mut self,
        name: UserName,
        mechanism: SaslMechanism,
        login_ms: u64,
    ) -> Result<(), SaslError> {
        if self.user.as_ref().is_some_and(|u| u.name != name) {
            return Err(SaslError::AuthenticationFailed);
        }
        self.user = Some(Authenticated { name, mechanism, login_ms, roles: None, tenant: 0 });
        Ok(())
    }
}

/// Records that `name` authenticated, if its `authenticationRestrictions` and those of the roles
/// it inherits let it in from the connection's addresses.
fn admit<B: Backend + ?Sized>(
    state: &mut AuthState,
    backend: &mut B,
    name: UserName,
    mechanism: SaslMechanism,
    ldap_roles: Option<Vec<RoleName>>,
) -> Result<(), CommandError> {
    // `__system`, logged in with a member's certificate, may do anything.
    let ldap_roles = if is_internal(&name) {
        Some(vec![RoleName::new(SYSTEM_ROLE, ADMIN_DB)])
    } else {
        ldap_roles
    };
    let (own, roles, tenant) = match &ldap_roles {
        // Roles of LDAP groups: the user's document, if any, is not read (013 FR-026).
        Some(r) => (None, Some(r.clone()), 0),
        None => match login_user(backend, &name)? {
            Some(user) => (user.restrictions, Some(user.roles), user.tenant),
            None => (None, None, 0),
        },
    };
    // A user of a suspended tenant does not sign in (007 FR-034).
    if tenant != 0 && backend.tenant_state(tenant).is_some_and(|s| s.suspended) {
        return Err(CommandError::new(AUTHENTICATION_FAILED, TENANT_SUSPENDED));
    }
    if let Some(roles) = roles {
        let inherited =
            super::roles::Graph::new(&mut *backend).inherit(&roles, None).map_err(login_fault)?;
        let (client, server) = (state.client_addr.as_deref(), state.server_addr.as_deref());
        if !restrictions::allows(own.as_deref(), &inherited.restrictions, client, server) {
            return Err(sasl_error(&SaslError::AuthenticationFailed));
        }
    }
    state.login(name, mechanism, backend.now_ms()).map_err(|e| sasl_error(&e))?;
    if let Some(u) = state.user.as_mut() {
        u.roles = ldap_roles;
        u.tenant = tenant;
    }
    Ok(())
}

/// The message of an authentication of a user of a suspended tenant (007 FR-034).
pub const TENANT_SUSPENDED: &str = "tenant is suspended";

/// The user members of the cluster log in as (`__system` of `local`), and its role.
pub const INTERNAL_USER: &str = "__system";
const LOCAL_DB: &str = "local";
const SYSTEM_ROLE: &str = "__system";
const ADMIN_DB: &str = "admin";

/// Whether `name` is the cluster's internal user.
pub fn is_internal(name: &UserName) -> bool {
    name.user == INTERNAL_USER && name.db == LOCAL_DB
}

fn sasl_error(e: &SaslError) -> CommandError {
    let code = match e {
        SaslError::UnknownMechanism(_)
        | SaslError::NotEnabled(_)
        | SaslError::WrongDatabase(_)
        | SaslError::ExternalOnly(..)
        | SaslError::NoCredentials(_) => MECHANISM_UNAVAILABLE,
        SaslError::AuthenticationFailed => AUTHENTICATION_FAILED,
        SaslError::NoConversation => PROTOCOL_ERROR,
        SaslError::UserNotFound(_) => USER_NOT_FOUND,
    };
    CommandError::new(code, e.to_string())
}

fn missing(cmd: &str, field: &str) -> CommandError {
    let msg = format!("BSON field '{cmd}.{field}' is missing but a required field");
    CommandError::new(IDL_FAILED_TO_PARSE, msg)
}

/// A SASL payload: binary data or a Base64 string, answered the way it came.
fn payload(v: &Value) -> Result<(Vec<u8>, bool), CommandError> {
    match v {
        Value::Binary { bytes, .. } => Ok((bytes.clone(), false)),
        Value::String(s) => base64_decode_strict(s.as_bytes()).map(|b| (b, true)).map_err(|_| {
            CommandError::new(
                SASL_PAYLOAD_NOT_BASE64,
                "Failed decoding SASL payload: invalid base64",
            )
        }),
        other => {
            let msg = format!(
                "Invalid SASLPayload type. Expected Base64 or BinData, got: {}",
                type_name(other)
            );
            Err(CommandError::new(BAD_VALUE, msg))
        }
    }
}

fn payload_value(bytes: Vec<u8>, as_string: bool) -> Value {
    if as_string {
        Value::String(base64_encode(&bytes))
    } else {
        Value::Binary { subtype: 0, bytes }
    }
}

fn conversation_reply(done: bool, bytes: Vec<u8>, as_string: bool) -> Document {
    Document::new()
        .with("conversationId", CONVERSATION_ID)
        .with("done", done)
        .with("payload", payload_value(bytes, as_string))
        .with("ok", 1.0)
}

/// The fields of `saslStart` once checked.
struct StartArgs {
    mechanism: String,
    payload: Vec<u8>,
    as_string: bool,
    skip_empty: bool,
}

fn start_args(cmd: &Document) -> Result<StartArgs, CommandError> {
    let (mut mechanism, mut data, mut skip_empty) = (None, None, false);
    for (k, v) in cmd.iter().skip(1) {
        let path = format!("{SASL_START}.{k}");
        match k {
            "mechanism" => match v {
                Value::String(m) => mechanism = Some(m.clone()),
                other => return Err(wrong_type(&path, other, "string")),
            },
            "payload" => data = Some(payload(v)?),
            "autoAuthorize" => {
                truthy(v).ok_or_else(|| wrong_types(&path, v, &AUTO_AUTHORIZE_TYPES))?;
            }
            "options" => match v {
                Value::Document(o) => {
                    skip_empty = o.get("skipEmptyExchange").and_then(truthy).unwrap_or(false);
                }
                other => return Err(wrong_type(&path, other, "object")),
            },
            _ => {}
        }
    }
    let mechanism = mechanism.ok_or_else(|| missing(SASL_START, "mechanism"))?;
    let (payload, as_string) = data.ok_or_else(|| missing(SASL_START, "payload"))?;
    Ok(StartArgs { mechanism, payload, as_string, skip_empty })
}

/// `saslStart` on database `db`; a new conversation replaces the one in progress.
fn sasl_start<B: Backend + ?Sized>(
    state: &mut AuthState,
    db: &str,
    cmd: &Document,
    backend: &mut B,
) -> Result<Document, CommandError> {
    let args = start_args(cmd)?;
    state.conversation = None;
    state.aws = None;
    state.oidc = None;
    state.gss = None;
    let mech = sasl::start_mechanism(&args.mechanism, db, &state.mechanisms)
        .map_err(|e| sasl_error(&e))?;
    if mech == SaslMechanism::Aws {
        state.attempt = Some((UserName::new("", db), mech.name().to_owned()));
        let failed = |_| sasl_error(&SaslError::AuthenticationFailed);
        let host = aws::sts_host(&backend.aws_sts_url()).map_err(failed)?;
        let mut random = [0u8; aws::NONCE_LEN];
        backend.secure_random(&mut random)?;
        let (server, reply) = aws::Server::start(&args.payload, random, &host).map_err(failed)?;
        state.aws = Some((server, args.as_string));
        return Ok(conversation_reply(false, reply, args.as_string));
    }
    if mech == SaslMechanism::Oidc {
        state.attempt = Some((UserName::new("", db), mech.name().to_owned()));
        let failed = |_| sasl_error(&SaslError::AuthenticationFailed);
        return match oidc::read_message(&args.payload).map_err(failed)? {
            oidc::First::Jwt(jwt) => {
                oidc_login(state, &jwt, backend)?;
                Ok(conversation_reply(true, Vec::new(), args.as_string))
            }
            oidc::First::Ask(name) => {
                let providers = backend.oidc_providers();
                let reply = oidc::provider_reply(&providers, name.as_deref()).map_err(failed)?;
                state.oidc = Some(args.as_string);
                Ok(conversation_reply(false, reply, args.as_string))
            }
        };
    }
    if mech == SaslMechanism::Gssapi {
        state.attempt = Some((UserName::new("", db), mech.name().to_owned()));
        let failed = |_| sasl_error(&SaslError::AuthenticationFailed);
        let mut confounder = [0u8; 16];
        backend.secure_random(&mut confounder)?;
        let now = i64::try_from(backend.now_ms() / 1000).unwrap_or(i64::MAX);
        let keys = backend.kerberos_keys();
        let accepted = kerberos::accept(&args.payload, &keys, now, confounder).map_err(failed)?;
        state.attempt = Some((accepted.user.clone(), mech.name().to_owned()));
        let mut context = accepted.context;
        // With mutual authentication the AP-REP goes first and the offer after the client's
        // empty answer; without it, the offer at once (RFC 4752).
        let (reply, offered) = match accepted.reply {
            Some(rep) => (rep, false),
            None => (context.wrap(&kerberos::layer_offer()).map_err(failed)?, true),
        };
        state.gss = Some((context, accepted.user, offered, args.as_string));
        return Ok(conversation_reply(false, reply, args.as_string));
    }
    if mech == SaslMechanism::Plain {
        let (name, roles) = plain_login(state, db, &args.payload, backend)?;
        admit(state, backend, name, mech, roles)?;
        return Ok(conversation_reply(true, Vec::new(), args.as_string));
    }
    let Some(scram) = mech.scram() else {
        // REFDB-X509: the certificate is the whole proof.
        let name =
            sasl::x509_user(db, None, state.peer_user.as_deref()).map_err(|e| sasl_error(&e))?;
        state.attempt = Some((name.clone(), mech.name().to_owned()));
        let (name, groups) = x509_roles(state, backend, name, true)?;
        admit(state, backend, name, mech, groups)?;
        return Ok(conversation_reply(true, Vec::new(), args.as_string));
    };
    let (first, name) = sasl::scram_user(db, &args.payload).map_err(|e| sasl_error(&e))?;
    state.attempt = Some((name.clone(), mech.name().to_owned()));
    let user = login_user(backend, &name)?;
    let mut random = [0u8; SERVER_NONCE_BYTES];
    backend.secure_random(&mut random)?;
    let nonce = sasl::server_nonce(&random);
    let (conv, reply) = Conversation::start_scram(scram, &first, user, &nonce, args.skip_empty)
        .map_err(|e| sasl_error(&e))?;
    state.conversation = Some((conv, args.as_string));
    Ok(conversation_reply(false, reply, args.as_string))
}

/// `saslContinue`.
fn sasl_continue<B: Backend + ?Sized>(
    state: &mut AuthState,
    cmd: &Document,
    backend: &mut B,
) -> Result<Document, CommandError> {
    let (mut id, mut data) = (None, None);
    for (k, v) in cmd.iter().skip(1) {
        match k {
            "conversationId" => match v {
                Value::Int32(n) => id = Some(*n),
                other => {
                    return Err(wrong_type(&format!("{SASL_CONTINUE}.{k}"), other, "int"));
                }
            },
            "payload" => data = Some(payload(v)?),
            _ => {}
        }
    }
    let id = id.ok_or_else(|| missing(SASL_CONTINUE, "conversationId"))?;
    let (bytes, _) = data.ok_or_else(|| missing(SASL_CONTINUE, "payload"))?;
    let no_state = || sasl_error(&SaslError::NoConversation);
    if let Some((mut context, user, offered, as_string)) = state.gss.take() {
        if id != CONVERSATION_ID {
            state.gss = Some((context, user, offered, as_string));
            return Err(no_state());
        }
        let failed = |_| sasl_error(&SaslError::AuthenticationFailed);
        if !offered {
            let offer = context.wrap(&kerberos::layer_offer()).map_err(failed)?;
            state.gss = Some((context, user, true, as_string));
            return Ok(conversation_reply(false, offer, as_string));
        }
        let answer = context.unwrap(&bytes).map_err(failed)?;
        kerberos::layer_answer(&answer, &user).map_err(failed)?;
        if login_user(backend, &user)?.is_none() {
            return Err(sasl_error(&SaslError::AuthenticationFailed));
        }
        admit(state, backend, user, SaslMechanism::Gssapi, None)?;
        return Ok(conversation_reply(true, Vec::new(), as_string));
    }
    if let Some(as_string) = state.oidc.take() {
        if id != CONVERSATION_ID {
            state.oidc = Some(as_string);
            return Err(no_state());
        }
        let failed = |_| sasl_error(&SaslError::AuthenticationFailed);
        let oidc::First::Jwt(jwt) = oidc::read_message(&bytes).map_err(failed)? else {
            return Err(sasl_error(&SaslError::AuthenticationFailed));
        };
        oidc_login(state, &jwt, backend)?;
        return Ok(conversation_reply(true, Vec::new(), as_string));
    }
    if let Some((server, as_string)) = state.aws.take() {
        if id != CONVERSATION_ID {
            state.aws = Some((server, as_string));
            return Err(no_state());
        }
        let name = aws_login(&server, &bytes, backend)?;
        state.attempt = Some((name.clone(), SaslMechanism::Aws.name().to_owned()));
        admit(state, backend, name, SaslMechanism::Aws, None)?;
        return Ok(conversation_reply(true, Vec::new(), as_string));
    }
    let (mut conv, as_string) = match state.conversation.take() {
        Some(c) if id == CONVERSATION_ID => c,
        other => {
            state.conversation = other;
            return Err(no_state());
        }
    };
    match conv.step(&bytes).map_err(|e| sasl_error(&e))? {
        Step::Continue(reply) => {
            state.conversation = Some((conv, as_string));
            Ok(conversation_reply(false, reply, as_string))
        }
        Step::Done { payload, user } => {
            admit(state, backend, user, conv_mechanism(&conv), None)?;
            Ok(conversation_reply(true, payload, as_string))
        }
    }
}

fn conv_mechanism(conv: &Conversation) -> SaslMechanism {
    match conv.mechanism() {
        teseladb_auth::scram::Mechanism::ScramSha1 => SaslMechanism::ScramSha1,
        teseladb_auth::scram::Mechanism::ScramSha256 => SaslMechanism::ScramSha256,
    }
}

/// The roles of a REFDB-X509 user: with LDAP authorization its LDAP groups, without its
/// document in `$external` being read or needed, whatever `useInternalAuthzForX509` says
/// (measured on RefDb Enterprise 9.0.4, 013 FR-033); otherwise its document, which must exist.
fn x509_roles<B: Backend + ?Sized>(
    state: &AuthState,
    backend: &mut B,
    name: UserName,
    via_sasl: bool,
) -> Result<(UserName, Option<Vec<RoleName>>), CommandError> {
    // A member's certificate, while `clusterAuthMode` takes them, is `__system` (measured on
    // `refnode` 9.0.4 with `sendX509` and `x509`).
    if state.peer_member && state.members_by_x509 {
        return Ok((UserName::new(INTERNAL_USER, LOCAL_DB), None));
    }
    if let Some(groups) = backend.ldap_groups(&name.user)? {
        return Ok((name, Some(groups)));
    }
    let exists = login_user(backend, &name)?.is_some();
    let name = sasl::x509_found(name, exists, via_sasl).map_err(|e| sasl_error(&e))?;
    Ok((name, None))
}

/// `authenticate`: only REFDB-X509 (any other mechanism fails as on RefDb 9.0).
fn authenticate<B: Backend + ?Sized>(
    state: &mut AuthState,
    db: &str,
    cmd: &Document,
    backend: &mut B,
) -> Result<Document, CommandError> {
    let (mut mechanism, mut requested) = (None, None);
    for (k, v) in cmd.iter().skip(1) {
        let path = format!("{AUTHENTICATE}.{k}");
        match (k, v) {
            ("mechanism", Value::String(m)) => mechanism = Some(m.clone()),
            ("user", Value::String(u)) => requested = Some(u.clone()),
            ("mechanism" | "user", other) => return Err(wrong_type(&path, other, "string")),
            _ => {}
        }
    }
    let mechanism = mechanism.ok_or_else(|| missing(AUTHENTICATE, "mechanism"))?;
    if SaslMechanism::from_name(&mechanism) != Some(SaslMechanism::X509) {
        return Err(sasl_error(&SaslError::AuthenticationFailed));
    }
    let name = sasl::x509_user(db, requested.as_deref(), state.peer_user.as_deref())
        .map_err(|e| sasl_error(&e))?;
    state.attempt = Some((name.clone(), SaslMechanism::X509.name().to_owned()));
    // The answer names the certificate's user, also when it logs in as `__system` (measured).
    let shown = name.clone();
    let (name, groups) = x509_roles(state, backend, name, false)?;
    admit(state, backend, name, SaslMechanism::X509, groups)?;
    Ok(Document::new().with("dbname", shown.db).with("user", shown.user).with("ok", 1.0))
}

/// Runs authentication command `name` (one of [`is_auth_command`]) on database `db`.
pub fn run<B: Backend + ?Sized>(
    state: &mut AuthState,
    db: &str,
    cmd: &Document,
    backend: &mut B,
) -> Result<Document, CommandError> {
    match cmd.iter().next().map_or("", |(k, _)| k) {
        SASL_START => sasl_start(state, db, cmd, backend),
        SASL_CONTINUE => sasl_continue(state, cmd, backend),
        _ => authenticate(state, db, cmd, backend),
    }
}

/// A stored user or role RefDb would not use fails a login as any other failure does
/// (measured on RefDb 9.0.4: `hello` names the fault, `saslStart` does not); errors of the
/// node itself go through.
fn login_fault(e: CommandError) -> CommandError {
    match STORED_FAULTS.contains(&e.code.code) {
        true => sasl_error(&SaslError::AuthenticationFailed),
        false => e,
    }
}

/// User `name` acquired for a login: see [`acquire`] and [`login_fault`].
fn login_user<B: Backend + ?Sized>(
    backend: &mut B,
    name: &UserName,
) -> Result<Option<User>, CommandError> {
    acquire(backend, name).map_err(login_fault)
}

/// The codes of a stored user or role document RefDb would not use.
const STORED_FAULTS: [i32; 10] = [
    teseladb_auth::users::BAD_VALUE,
    teseladb_auth::users::UNSUPPORTED_FORMAT,
    teseladb_auth::users::INVALID_UUID,
    teseladb_auth::users::EXPECTED_OBJECT,
    teseladb_auth::users::BAD_ITERATION_COUNT,
    teseladb_auth::users::MISSING_SALT,
    teseladb_auth::users::MISSING_SERVER_KEY,
    teseladb_auth::users::MISSING_STORED_KEY,
    teseladb_auth::users::CREDENTIALS_INVALID,
    teseladb_auth::users::MISSING_FIELD,
];

/// User `name` as RefDb acquires it to log in: its document and the roles it inherits, so that
/// a stored user or role it would not use is an error with RefDb's code and words (measured on
/// `hello` with `saslSupportedMechs`).
pub fn acquire<B: Backend + ?Sized>(
    backend: &mut B,
    name: &UserName,
) -> Result<Option<User>, CommandError> {
    let user = backend.user(name)?;
    if let Some(u) = &user {
        super::roles::user_inherited(backend, u)?;
    }
    Ok(user)
}

/// The `speculativeAuthenticate` of `hello`, already validated: its `saslStart` or
/// `authenticate` run on its `db`. The reply without `ok`, or `None` if it failed: the field is
/// then left out of `hello` and the client authenticates the usual way (measured).
pub fn speculative<B: Backend + ?Sized>(
    state: &mut AuthState,
    spec: &Document,
    backend: &mut B,
) -> Option<Document> {
    let Some(Value::String(db)) = spec.get("db") else { return None };
    let db = db.clone();
    let mut cmd = spec.clone();
    cmd.remove("db");
    let name = cmd.iter().next().map(|(k, _)| k.to_owned())?;
    crate::generic::check_unknown_fields(&cmd, &name).ok()?;
    let result = match name.as_str() {
        SASL_START => sasl_start(state, &db, &cmd, backend),
        _ => authenticate(state, &db, &cmd, backend),
    };
    let mut doc = result.ok()?;
    doc.remove("ok");
    Some(doc)
}

/// REFDB-OIDC with the access token `jwt` (013 FR-025): the token checked against its
/// provider's JWKS (read again once for a key it does not know), then the user logged in with the
/// roles of its authorization claim, or with its document in `$external` without one. Every
/// failure is `Authentication failed.`, as RefDb Enterprise 9.0.4 answers it.
fn oidc_login<B: Backend + ?Sized>(
    state: &mut AuthState,
    jwt: &str,
    backend: &mut B,
) -> Result<(), CommandError> {
    let failed = || sasl_error(&SaslError::AuthenticationFailed);
    let token = oidc::Token::parse(jwt).map_err(|_| failed())?;
    let providers = backend.oidc_providers();
    let now = i64::try_from(backend.now_ms() / 1000).unwrap_or(i64::MAX);
    let keys = backend.oidc_keys(&token.issuer, false).map_err(|_| failed())?;
    let resource = state.oidc_resource.as_deref();
    let identity = match token.verify_for(&providers, &keys, now, resource) {
        Err(e) if e.0.starts_with("Unknown JWT keyId") => {
            let keys = backend.oidc_keys(&token.issuer, true).map_err(|_| failed())?;
            token.verify_for(&providers, &keys, now, resource)
        }
        other => other,
    }
    .map_err(|_| failed())?;
    state.attempt = Some((identity.user.clone(), SaslMechanism::Oidc.name().to_owned()));
    if identity.roles.is_none() && login_user(backend, &identity.user)?.is_none() {
        return Err(failed());
    }
    admit(state, backend, identity.user, SaslMechanism::Oidc, identity.roles)
}

/// Logs in with API key `key` (019 FR-015, FR-017): the key found by its SHA-256, not expired,
/// its user still there and let in from the connection's addresses, and the roles of the key the
/// user still holds as the connection's roles. Every failure is `Authentication failed.`. The key
/// found, for its last use.
pub fn api_key_login<B: Backend + ?Sized>(
    state: &mut AuthState,
    key: &str,
    backend: &mut B,
) -> Result<teseladb_auth::apikey::ApiKey, CommandError> {
    use teseladb_auth::apikey::{self, ApiKey};
    let failed = || sasl_error(&SaslError::AuthenticationFailed);
    if !apikey::is_key(key) {
        return Err(failed());
    }
    let id = teseladb_meta::security::string_id(&apikey::hash_hex(key));
    let doc = backend.principal_by_id(teseladb_meta::security::Principal::ApiKey, &id)?;
    let doc = doc.ok_or_else(failed)?;
    let raw = teseladb_bson::RawDoc::from_bytes(&doc).map_err(|_| failed())?;
    let found = ApiKey::from_document(&Document::from(raw)).map_err(|_| failed())?;
    let name = UserName::new(found.user.as_str(), found.db.as_str());
    state.attempt = Some((name.clone(), SaslMechanism::ApiKey.name().to_owned()));
    let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
    if found.expired(now) {
        return Err(failed());
    }
    let Some(user) = login_user(backend, &name)? else { return Err(failed()) };
    admit(state, backend, name, SaslMechanism::ApiKey, None)?;
    if let Some(u) = state.user.as_mut() {
        u.roles = Some(found.effective_roles(&user.roles));
    }
    Ok(found)
}

/// The end of REFDB-AWS (013 FR-025): the client's signed request goes to the STS, whose
/// answer names the user, which must exist in `$external`. Every failure is `Authentication
/// failed.`, as RefDb Enterprise 9.0.4 answers it.
fn aws_login<B: Backend + ?Sized>(
    server: &aws::Server,
    payload: &[u8],
    backend: &mut B,
) -> Result<UserName, CommandError> {
    let failed = || sasl_error(&SaslError::AuthenticationFailed);
    let req = server.request(payload).map_err(|_| failed())?;
    // An STS that cannot be reached is a failed login too; the node logs why.
    let (status, body) = backend.aws_sts(&req).map_err(|_| failed())?;
    let arn = aws::caller(status, &body).map_err(|_| failed())?;
    let name = UserName::new(arn, EXTERNAL_DB);
    if login_user(backend, &name)?.is_none() {
        return Err(failed());
    }
    Ok(name)
}

/// PLAIN (013 FR-025): on `$external` the LDAP server checks the password and the user must
/// exist in `$external`, unless the node authorizes by LDAP groups (FR-026): then the groups are
/// its roles and its document, if any, is not read. On another database the password is checked
/// against the user's SCRAM credentials. Measured on RefDb Enterprise 9.0.4; any failure is
/// `Authentication failed.`.
fn plain_login<B: Backend + ?Sized>(
    state: &mut AuthState,
    db: &str,
    payload: &[u8],
    backend: &mut B,
) -> Result<(UserName, Option<Vec<RoleName>>), CommandError> {
    let failed = || sasl_error(&SaslError::AuthenticationFailed);
    let p = teseladb_auth::plain::parse(payload).map_err(|_| failed())?;
    let name = UserName::new(p.user.as_str(), db);
    state.attempt = Some((name.clone(), SaslMechanism::Plain.name().to_owned()));
    if db == EXTERNAL_DB {
        return match backend.ldap_login(&p.user, &p.password)? {
            LdapLogin::Authorized(roles) => Ok((name, Some(roles))),
            LdapLogin::LoggedIn if login_user(backend, &name)?.is_some() => Ok((name, None)),
            LdapLogin::LoggedIn | LdapLogin::Refused => Err(failed()),
        };
    }
    let ok = login_user(backend, &name)?
        .is_some_and(|u| teseladb_auth::plain::check_local(&u, &p.password));
    if ok { Ok((name, None)) } else { Err(failed()) }
}
