//! The server side of RefDb's SASL conversations (FR-008), without BSON: which mechanisms
//! exist, the SCRAM conversation of `saslStart` and `saslContinue` with its optional empty last
//! step, REFDB-X509 against the client certificate, and the errors with RefDb 9.0.4's
//! messages (measured on a `refrouter` with `--keyFile` and on a `refnode` with TLS).

use std::fmt;

use teseladb_x509::pem::base64_encode;

use crate::scram::{ClientFirst, Mechanism, ScramError, ServerSession};
use crate::users::{EXTERNAL_DB, User, UserName};

/// The only conversation a connection holds: RefDb numbers it 1 (measured).
pub const CONVERSATION_ID: i32 = 1;
/// Random bytes of the server's part of the SCRAM nonce: RefDb sends 32 Base64 characters.
pub const SERVER_NONCE_BYTES: usize = 24;

/// A SASL mechanism RefDb knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SaslMechanism {
    /// SCRAM-SHA-1.
    ScramSha1,
    /// SCRAM-SHA-256.
    ScramSha256,
    /// REFDB-X509.
    X509,
    /// PLAIN (LDAP, FR-025).
    Plain,
    /// GSSAPI (Kerberos, FR-025).
    Gssapi,
    /// REFDB-AWS (FR-025).
    Aws,
    /// REFDB-OIDC (FR-025).
    Oidc,
    /// An API key of the HTTP API (019 FR-015); no command of the wire protocol takes it, so it
    /// is not in [`SaslMechanism::ALL`].
    ApiKey,
}

impl SaslMechanism {
    /// Every mechanism.
    pub const ALL: [SaslMechanism; 7] = [
        SaslMechanism::ScramSha1,
        SaslMechanism::ScramSha256,
        SaslMechanism::X509,
        SaslMechanism::Plain,
        SaslMechanism::Gssapi,
        SaslMechanism::Aws,
        SaslMechanism::Oidc,
    ];

    /// The name clients send (case matters, as in RefDb).
    pub fn name(self) -> &'static str {
        match self {
            SaslMechanism::ScramSha1 => Mechanism::ScramSha1.name(),
            SaslMechanism::ScramSha256 => Mechanism::ScramSha256.name(),
            SaslMechanism::X509 => teseladb_names::SASL_X509,
            SaslMechanism::Plain => "PLAIN",
            SaslMechanism::Gssapi => "GSSAPI",
            SaslMechanism::Aws => teseladb_names::SASL_AWS,
            SaslMechanism::Oidc => teseladb_names::SASL_OIDC,
            SaslMechanism::ApiKey => crate::apikey::MECHANISM,
        }
    }

    /// The mechanism called `name`.
    pub fn from_name(name: &str) -> Option<SaslMechanism> {
        SaslMechanism::ALL.into_iter().find(|m| m.name() == name)
    }

    /// Whether the node takes it by default: the default `authenticationMechanisms` of RefDb
    /// 9.0 (SCRAM-SHA-1, SCRAM-SHA-256 and REFDB-X509).
    pub fn enabled(self) -> bool {
        matches!(self, SaslMechanism::ScramSha1 | SaslMechanism::ScramSha256 | SaslMechanism::X509)
    }

    /// Whether the node takes it, given `authenticationMechanisms` (empty: the default).
    pub fn is_enabled(self, enabled: &[SaslMechanism]) -> bool {
        if enabled.is_empty() { self.enabled() } else { enabled.contains(&self) }
    }

    /// The SCRAM mechanism, if it is one.
    pub fn scram(self) -> Option<Mechanism> {
        match self {
            SaslMechanism::ScramSha1 => Some(Mechanism::ScramSha1),
            SaslMechanism::ScramSha256 => Some(Mechanism::ScramSha256),
            _ => None,
        }
    }
}

/// Why authentication failed; [`fmt::Display`] gives RefDb's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaslError {
    /// A name RefDb does not know (`MechanismUnavailable`).
    UnknownMechanism(String),
    /// A mechanism the node does not take (`MechanismUnavailable`).
    NotEnabled(SaslMechanism),
    /// A SCRAM mechanism on `$external` (`MechanismUnavailable`).
    WrongDatabase(SaslMechanism),
    /// A mechanism of `$external` alone on database `db` (`MechanismUnavailable`).
    ExternalOnly(SaslMechanism, String),
    /// A user without credentials for the mechanism (`MechanismUnavailable`).
    NoCredentials(Mechanism),
    /// Wrong password, unknown user, bad message (`AuthenticationFailed`): the client learns
    /// nothing else.
    AuthenticationFailed,
    /// `saslContinue` without that conversation (`ProtocolError`).
    NoConversation,
    /// `authenticate` with REFDB-X509 for a certificate without its user (`UserNotFound`).
    UserNotFound(UserName),
}

impl fmt::Display for SaslError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaslError::UnknownMechanism(m) => {
                write!(f, "Received authentication for mechanism {m} which is not known")
            }
            SaslError::NotEnabled(m) => {
                write!(f, "Received authentication for mechanism {} which is not enabled", m.name())
            }
            SaslError::WrongDatabase(m) => write!(
                f,
                "Unsupported mechanism '{}' on authentication database '{EXTERNAL_DB}'",
                m.name()
            ),
            SaslError::ExternalOnly(m, db) => {
                write!(f, "Unsupported mechanism '{}' on authentication database '{db}'", m.name())
            }
            SaslError::NoCredentials(m) => write!(
                f,
                "Unable to use {0} based authentication for user without any {0} credentials registered",
                m.name()
            ),
            SaslError::AuthenticationFailed => f.write_str("Authentication failed."),
            SaslError::NoConversation => f.write_str("No SASL session state found"),
            SaslError::UserNotFound(u) => {
                write!(f, "Could not find user \"{}\" for db \"{}\"", u.user, u.db)
            }
        }
    }
}

impl std::error::Error for SaslError {}

impl From<ScramError> for SaslError {
    /// Every SCRAM failure reaches the client as `Authentication failed.`.
    fn from(_: ScramError) -> SaslError {
        SaslError::AuthenticationFailed
    }
}

/// The mechanism of `saslStart` on database `db`.
pub fn start_mechanism(
    name: &str,
    db: &str,
    enabled: &[SaslMechanism],
) -> Result<SaslMechanism, SaslError> {
    let mech =
        SaslMechanism::from_name(name).ok_or_else(|| SaslError::UnknownMechanism(name.into()))?;
    if !mech.is_enabled(enabled) {
        return Err(SaslError::NotEnabled(mech));
    }
    if mech.scram().is_some() && db == EXTERNAL_DB {
        return Err(SaslError::WrongDatabase(mech));
    }
    if matches!(mech, SaslMechanism::Aws | SaslMechanism::Oidc | SaslMechanism::Gssapi)
        && db != EXTERNAL_DB
    {
        return Err(SaslError::ExternalOnly(mech, db.to_owned()));
    }
    Ok(mech)
}

/// What a step of a conversation answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The payload to send; the conversation goes on.
    Continue(Vec<u8>),
    /// The payload to send; `user` is authenticated.
    Done {
        /// Last payload.
        payload: Vec<u8>,
        /// Who authenticated.
        user: UserName,
    },
}

#[derive(Debug)]
enum State {
    /// The server-first-message went out.
    Scram { session: ServerSession, user: UserName },
    /// The server-final-message went out; an empty step ends it (no `skipEmptyExchange`).
    Final(UserName),
}

/// A SCRAM conversation after its `saslStart`.
#[derive(Debug)]
pub struct Conversation {
    mech: Mechanism,
    state: State,
    skip_empty: bool,
}

/// The user a SCRAM `client-first-message` names in database `db`, and the parsed message.
pub fn scram_user(db: &str, payload: &[u8]) -> Result<(ClientFirst, UserName), SaslError> {
    let first = ClientFirst::parse(payload)?;
    let name = UserName::new(first.user.clone(), db);
    Ok((first, name))
}

/// The server's part of the nonce from [`SERVER_NONCE_BYTES`] random bytes.
pub fn server_nonce(random: &[u8; SERVER_NONCE_BYTES]) -> String {
    base64_encode(random)
}

impl Conversation {
    /// Starts a SCRAM conversation for `first`, whose user is `user` (`None`: it does not
    /// exist); returns it with the `server-first-message`. With `skip_empty` the server-final
    /// message ends the conversation (`options.skipEmptyExchange`).
    pub fn start_scram(
        mech: Mechanism,
        first: &ClientFirst,
        user: Option<User>,
        nonce: &str,
        skip_empty: bool,
    ) -> Result<(Conversation, Vec<u8>), SaslError> {
        let user = user.ok_or(SaslError::AuthenticationFailed)?;
        let creds = user.credentials(mech).cloned().ok_or(SaslError::NoCredentials(mech))?;
        let (session, payload) = ServerSession::start(mech, first, creds, nonce);
        let state = State::Scram { session, user: user.name };
        Ok((Conversation { mech, state, skip_empty }, payload))
    }

    /// Its mechanism.
    pub fn mechanism(&self) -> Mechanism {
        self.mech
    }

    /// The next step with the client's `payload`. A failure ends the conversation.
    pub fn step(&mut self, payload: &[u8]) -> Result<Step, SaslError> {
        match &mut self.state {
            State::Scram { session, user } => {
                let last = session.finish(payload)?;
                let user = user.clone();
                if self.skip_empty {
                    return Ok(Step::Done { payload: last, user });
                }
                self.state = State::Final(user);
                Ok(Step::Continue(last))
            }
            // RefDb ends it whatever the client sends (measured with an empty payload).
            State::Final(user) => Ok(Step::Done { payload: Vec::new(), user: user.clone() }),
        }
    }
}

/// REFDB-X509 (`authenticate`, or `saslStart`) on database `db`: the user of the client
/// certificate `peer` (its RFC 2253 name), which must equal `requested` when the client names
/// one. [`x509_found`] then checks that the user exists.
pub fn x509_user(
    db: &str,
    requested: Option<&str>,
    peer: Option<&str>,
) -> Result<UserName, SaslError> {
    if db != EXTERNAL_DB {
        return Err(SaslError::AuthenticationFailed);
    }
    let peer = peer.ok_or(SaslError::AuthenticationFailed)?;
    if requested.is_some_and(|r| r != peer) {
        return Err(SaslError::AuthenticationFailed);
    }
    Ok(UserName::new(peer, EXTERNAL_DB))
}

/// The end of REFDB-X509 for `name`, whose user exists or not: through `saslStart`
/// (`via_sasl`) a missing user is `AuthenticationFailed`, through `authenticate` `UserNotFound`
/// (measured).
pub fn x509_found(name: UserName, exists: bool, via_sasl: bool) -> Result<UserName, SaslError> {
    match (exists, via_sasl) {
        (true, _) => Ok(name),
        (false, true) => Err(SaslError::AuthenticationFailed),
        (false, false) => Err(SaslError::UserNotFound(name)),
    }
}
