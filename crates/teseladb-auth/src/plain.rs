//! The PLAIN mechanism (RFC 4616; 013 FR-025) as RefDb Enterprise 9.0.4 takes it (measured):
//! `[authzid] NUL authcid NUL password` with the authorization identity empty or the same user;
//! on `$external` the password goes to LDAP, on any other database it is checked against the
//! user's SCRAM credentials. Every failure reaches the client as `Authentication failed.`; the
//! reason is for the log.

use std::fmt;

use teseladb_crypto::ct;

use crate::scram::{Credentials, Mechanism};
use crate::users::User;

/// Why a PLAIN message was refused (RefDb's message for its log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlainError {
    /// No NUL before the user.
    MissingFirstNul,
    /// No NUL before the password.
    MissingSecondNul,
    /// An authorization identity that is not the user.
    OtherIdentity,
    /// No user.
    EmptyUser,
    /// No password.
    EmptyPassword,
    /// The user or password is not UTF-8.
    NotUtf8,
}

impl fmt::Display for PlainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            PlainError::MissingFirstNul => {
                "Incorrectly formatted PLAIN client message, missing first NULL delimiter"
            }
            PlainError::MissingSecondNul => {
                "Incorrectly formatted PLAIN client message, missing second NULL delimiter"
            }
            PlainError::OtherIdentity => {
                "SASL authorization identity must match authentication identity"
            }
            PlainError::EmptyUser => "Incorrectly formatted PLAIN client message, empty username",
            PlainError::EmptyPassword => {
                "Incorrectly formatted PLAIN client message, empty password"
            }
            PlainError::NotUtf8 => "Incorrectly formatted PLAIN client message, not UTF-8",
        };
        f.write_str(reason)
    }
}

impl std::error::Error for PlainError {}

/// A PLAIN message: the user and the password, wiped when dropped.
#[derive(PartialEq, Eq)]
pub struct Plain {
    /// The user.
    pub user: String,
    /// The password.
    pub password: Vec<u8>,
}

impl fmt::Debug for Plain {
    /// Without the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plain").field("user", &self.user).finish()
    }
}

impl Drop for Plain {
    fn drop(&mut self) {
        self.password.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.password);
    }
}

/// Reads a PLAIN message.
pub fn parse(payload: &[u8]) -> Result<Plain, PlainError> {
    let first = payload.iter().position(|&b| b == 0).ok_or(PlainError::MissingFirstNul)?;
    let rest = &payload[first + 1..];
    let second = rest.iter().position(|&b| b == 0).ok_or(PlainError::MissingSecondNul)?;
    let (authz, user, password) = (&payload[..first], &rest[..second], &rest[second + 1..]);
    if user.is_empty() {
        return Err(PlainError::EmptyUser);
    }
    if password.is_empty() {
        return Err(PlainError::EmptyPassword);
    }
    if !authz.is_empty() && authz != user {
        return Err(PlainError::OtherIdentity);
    }
    let user = std::str::from_utf8(user).map_err(|_| PlainError::NotUtf8)?;
    Ok(Plain { user: user.to_owned(), password: password.to_vec() })
}

fn matches(mech: Mechanism, creds: &Credentials, user: &str, password: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(password) else { return false };
    let Ok(prepared) = mech.prepare_password(user, text, mech == Mechanism::ScramSha1) else {
        return false;
    };
    let derived = Credentials::derive(mech, &prepared, &creds.salt, creds.iterations);
    ct::ct_eq(&derived.stored_key, &creds.stored_key)
}

/// Whether `password` is the password of local user `user`: the SCRAM-SHA-256 credentials when
/// it has them, its SCRAM-SHA-1 ones otherwise.
pub fn check_local(user: &User, password: &[u8]) -> bool {
    let name = user.name.user.as_str();
    match (&user.scram_sha256, &user.scram_sha1) {
        (Some(c), _) => matches(Mechanism::ScramSha256, c, name, password),
        (None, Some(c)) => matches(Mechanism::ScramSha1, c, name, password),
        (None, None) => false,
    }
}
