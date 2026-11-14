//! Users (FR-008, FR-009, FR-013): a name in its database, the SCRAM credentials of each
//! mechanism (never a password), the roles granted to it, its `customData` and
//! `authenticationRestrictions`; and the document `admin.system.users` keeps for it (FR-030),
//! the form Group 0 stores, with the reference server 9.0.4's field order (measured).

use std::fmt;

use teseladb_bson::{Document, Value};
use teseladb_x509::pem::{base64_decode, base64_encode};

use crate::scram::{Credentials, Mechanism};

/// The database of the users that authenticate outside TeselaDB (X.509 authentication).
pub const EXTERNAL_DB: &str = "$external";

/// A user's name: the user and the database it was created in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserName {
    /// `user`.
    pub user: String,
    /// `db`.
    pub db: String,
}

impl UserName {
    /// User `user` of database `db`.
    pub fn new(user: impl Into<String>, db: impl Into<String>) -> UserName {
        UserName { user: user.into(), db: db.into() }
    }

    /// `db.user`, as `saslSupportedMechs` names a user: the database ends at the first dot.
    pub fn from_dotted(text: &str) -> Option<UserName> {
        let (db, user) = text.split_once('.')?;
        Some(UserName::new(user, db))
    }
}

impl fmt::Display for UserName {
    /// `db.user`, the `_id` of the user in `admin.system.users`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.db, self.user)
    }
}

/// A role's name: the role and its database.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleName {
    /// `role`.
    pub role: String,
    /// `db`.
    pub db: String,
}

impl RoleName {
    /// Role `role` of database `db`.
    pub fn new(role: impl Into<String>, db: impl Into<String>) -> RoleName {
        RoleName { role: role.into(), db: db.into() }
    }

    /// `{role, db}`.
    pub fn to_value(&self) -> Value {
        Value::Document(
            Document::new().with("role", self.role.as_str()).with("db", self.db.as_str()),
        )
    }

    /// Reads `{role, db}`.
    pub fn from_value(v: &Value) -> Option<RoleName> {
        let Value::Document(d) = v else { return None };
        match (d.get("role"), d.get("db")) {
            (Some(Value::String(r)), Some(Value::String(db))) => Some(RoleName::new(r, db)),
            _ => None,
        }
    }

    /// Reads a role name of a stored user or role as the reference server 9.0.4 does (measured):
    /// `{role, db}` with any other fields, or the string `db.role`.
    pub fn parse_stored(v: &Value) -> Result<RoleName, BadDocument> {
        let bad = |m: String| BadDocument::new(BAD_VALUE, m);
        match v {
            Value::String(s) => match s.split_once('.') {
                Some((db, role)) => Ok(RoleName::new(role, db)),
                None => Err(bad("RoleName must contain a '.' separated database.role pair".into())),
            },
            Value::Document(d) => {
                let text = |f: &str| match d.get(f) {
                    Some(Value::String(t)) => Ok(t.clone()),
                    None => Err(bad(format!("RoleName must contain a field named: {f}"))),
                    Some(other) => Err(bad(format!(
                        "RoleName must contain a string field named: {f}. But, has type {}",
                        teseladb_mql::types::name_of(other.element_type())
                    ))),
                };
                let role = text("role")?;
                Ok(RoleName::new(role, text("db")?))
            }
            _ => Err(bad("RoleName must be either a string or an object".into())),
        }
    }
}

/// A stored user or role document the reference server 9.0.4 would not use, with the code and
/// message it answers (measured on a reference replica set member with direct writes to
/// `admin.system.users` and `admin.system.roles`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadDocument {
    /// The error code.
    pub code: i32,
    /// The message.
    pub message: String,
}

impl BadDocument {
    /// Code `code` with `message`.
    pub fn new(code: i32, message: impl Into<String>) -> BadDocument {
        BadDocument { code, message: message.into() }
    }
}

/// `BadValue`.
pub const BAD_VALUE: i32 = 2;
/// `UserNotFound`: a document whose `user` or `db` is not a string names no user.
pub const USER_NOT_FOUND: i32 = 11;
/// `UnsupportedFormat`.
pub const UNSUPPORTED_FORMAT: i32 = 12;
/// `InvalidUUID`.
pub const INVALID_UUID: i32 = 207;
/// `Location10065`: a field that is not an object.
pub const EXPECTED_OBJECT: i32 = 10065;
/// `Location17501`: a SCRAM credential without a valid iteration count.
pub const BAD_ITERATION_COUNT: i32 = 17501;
/// `Location17502`, `Location17503`, `Location17504`: a SCRAM credential without salt, server
/// key or stored key.
pub const MISSING_SALT: i32 = 17502;
/// See [`MISSING_SALT`].
pub const MISSING_SERVER_KEY: i32 = 17503;
/// See [`MISSING_SALT`].
pub const MISSING_STORED_KEY: i32 = 17504;
/// `Location50684`: a SCRAM credential that is not Base64 of the right length.
pub const CREDENTIALS_INVALID: i32 = 50684;
/// `Location40414`: a required field is missing.
pub const MISSING_FIELD: i32 = 40414;

fn expected_object(what: &str) -> BadDocument {
    BadDocument::new(EXPECTED_OBJECT, format!("invalid parameter: expected an object ({what})"))
}

/// A user.
#[derive(Clone, Debug, PartialEq)]
pub struct User {
    /// Its name.
    pub name: UserName,
    /// `userId`, a UUID given when it was created; a document written directly may lack it.
    pub user_id: Option<[u8; 16]>,
    /// SCRAM-SHA-1 credentials, if the user has them.
    pub scram_sha1: Option<Credentials>,
    /// SCRAM-SHA-256 credentials, if the user has them.
    pub scram_sha256: Option<Credentials>,
    /// The roles granted to it, in the order they were granted.
    pub roles: Vec<RoleName>,
    /// `authenticationRestrictions`, as given.
    pub restrictions: Option<Vec<Value>>,
    /// `customData`.
    pub custom_data: Option<Document>,
    /// Its tenant (TeselaDB 007 FR-025): the one of the session that created it, 0 without
    /// tenants. Its document keeps it in [`TENANT_FIELD`] only when it is not 0.
    pub tenant: u32,
}

/// The field of a user document with its tenant, when it is not the default one (the field
/// `teseladb_meta::tenants::TENANT_FIELD` names too).
pub const TENANT_FIELD: &str = "tenantId";

/// A stored user document that the reference server would not use.
pub type BadUserDocument = BadDocument;

impl User {
    /// A user with no credentials and no roles (a user of `$external`).
    pub fn new(name: UserName) -> User {
        User {
            name,
            user_id: None,
            scram_sha1: None,
            scram_sha256: None,
            roles: Vec::new(),
            restrictions: None,
            custom_data: None,
            tenant: 0,
        }
    }

    /// The SASL mechanisms `usersInfo` lists for it: `external` for a user of `$external`.
    pub fn mechanisms(&self) -> Vec<&'static str> {
        if self.name.db == EXTERNAL_DB {
            return vec![EXTERNAL_CREDENTIALS];
        }
        [Mechanism::ScramSha1, Mechanism::ScramSha256]
            .into_iter()
            .filter(|m| self.credentials(*m).is_some())
            .map(Mechanism::name)
            .collect()
    }

    /// The `credentials` document: `{external: true}` for a user of `$external`, the SCRAM
    /// credentials of each mechanism otherwise.
    pub fn credentials_document(&self) -> Document {
        if self.name.db == EXTERNAL_DB {
            return Document::new().with(EXTERNAL_CREDENTIALS, true);
        }
        let mut creds = Document::new();
        for m in [Mechanism::ScramSha1, Mechanism::ScramSha256] {
            if let Some(c) = self.credentials(m) {
                let iterations = i32::try_from(c.iterations).unwrap_or(i32::MAX);
                let d = Document::new()
                    .with("iterationCount", iterations)
                    .with("salt", base64_encode(&c.salt))
                    .with("storedKey", base64_encode(&c.stored_key))
                    .with("serverKey", base64_encode(&c.server_key));
                creds.push(m.name(), d);
            }
        }
        creds
    }

    /// Its roles as `[{role, db}]`.
    pub fn roles_value(&self) -> Value {
        Value::Array(self.roles.iter().map(RoleName::to_value).collect())
    }

    /// The document of `admin.system.users`.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new().with("_id", self.name.to_string());
        if let Some(id) = &self.user_id {
            d.push("userId", Value::Binary { subtype: UUID_SUBTYPE, bytes: id.to_vec() });
        }
        let mut d = d
            .with("user", self.name.user.as_str())
            .with("db", self.name.db.as_str())
            .with("credentials", self.credentials_document());
        if self.tenant != 0 {
            d.push(TENANT_FIELD, i64::from(self.tenant));
        }
        if let Some(r) = &self.restrictions {
            d.push("authenticationRestrictions", Value::Array(r.clone()));
        }
        if let Some(c) = &self.custom_data {
            d.push("customData", c.clone());
        }
        d.push("roles", self.roles_value());
        d
    }

    /// Reads a stored user as the reference server 9.0.4 does when it loads one to log in
    /// (measured): a missing `userId`, a `customData` that is not a document and fields it does not
    /// know are fine; a field it needs in the wrong form is the error the reference server answers.
    pub fn from_document(d: &Document) -> Result<User, BadUserDocument> {
        let text = |f: &str| match d.get(f) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(BadDocument::new(USER_NOT_FOUND, format!("no user with a string '{f}'"))),
        };
        let mut u = User::new(UserName::new(text("user")?, text("db")?));
        u.user_id = match d.get("userId") {
            Some(Value::Binary { subtype: UUID_SUBTYPE, bytes }) => {
                let id: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                    BadDocument::new(
                        INVALID_UUID,
                        "uuid must be a 16-byte binary field with UUID (4) subtype",
                    )
                })?;
                Some(id)
            }
            _ => None,
        };
        let creds = match d.get("credentials") {
            Some(Value::Document(c)) => c,
            Some(_) => return Err(expected_object("credentials")),
            None => {
                return Err(BadDocument::new(
                    UNSUPPORTED_FORMAT,
                    "Cannot extract credentials from user documents without a 'credentials' field",
                ));
            }
        };
        if u.name.db != EXTERNAL_DB {
            u.scram_sha1 = read_credentials(creds, Mechanism::ScramSha1)?;
            u.scram_sha256 = read_credentials(creds, Mechanism::ScramSha256)?;
            if u.scram_sha1.is_none() && u.scram_sha256.is_none() {
                return Err(BadDocument::new(
                    UNSUPPORTED_FORMAT,
                    "User documents must provide credentials for SCRAM-SHA-1 and/or SCRAM-SHA-256",
                ));
            }
        }
        u.restrictions = match d.get("authenticationRestrictions") {
            None => None,
            Some(Value::Array(a)) => {
                check_restrictions(a)?;
                Some(a.clone())
            }
            Some(_) => {
                return Err(BadDocument::new(
                    UNSUPPORTED_FORMAT,
                    "'authenticationRestrictions' field must be an array",
                ));
            }
        };
        u.custom_data = match d.get("customData") {
            Some(Value::Document(c)) => Some(c.clone()),
            _ => None,
        };
        u.tenant = match d.get(TENANT_FIELD) {
            Some(Value::Int32(t)) => u32::try_from(*t).unwrap_or(0),
            Some(Value::Int64(t)) => u32::try_from(*t).unwrap_or(0),
            _ => 0,
        };
        u.roles = parse_roles(d)?;
        Ok(u)
    }

    /// The credentials of `mech`.
    pub fn credentials(&self, mech: Mechanism) -> Option<&Credentials> {
        match mech {
            Mechanism::ScramSha1 => self.scram_sha1.as_ref(),
            Mechanism::ScramSha256 => self.scram_sha256.as_ref(),
        }
    }
}

/// The `credentials` field of a user of `$external`, and the mechanism `usersInfo` lists for it.
pub const EXTERNAL_CREDENTIALS: &str = "external";
/// BSON binary subtype of a UUID.
pub const UUID_SUBTYPE: u8 = 4;

fn read_credentials(
    creds: &Document,
    m: Mechanism,
) -> Result<Option<Credentials>, BadUserDocument> {
    let Some(v) = creds.get(m.name()) else { return Ok(None) };
    let Value::Document(c) = v else { return Err(expected_object(m.name())) };
    let iterations = match c.get("iterationCount").and_then(number) {
        Some(n) if n >= 1.0 => n as u32,
        _ => {
            return Err(BadDocument::new(
                BAD_ITERATION_COUNT,
                format!("Invalid or missing {} iteration count", m.name()),
            ));
        }
    };
    let invalid = || {
        BadDocument::new(
            CREDENTIALS_INVALID,
            format!("credential document {} failed validation", m.name()),
        )
    };
    let field = |f: &str, code: i32, len: usize| match c.get(f) {
        Some(Value::String(s)) => match base64_decode(s.as_bytes()) {
            Ok(b) if b.len() == len => Ok(b),
            _ => Err(invalid()),
        },
        _ => Err(BadDocument::new(code, format!("Missing {} {f}", m.name()))),
    };
    let salt = field("salt", MISSING_SALT, m.salt_len())?;
    let server_key = field("serverKey", MISSING_SERVER_KEY, m.key_len())?;
    let stored_key = field("storedKey", MISSING_STORED_KEY, m.key_len())?;
    Ok(Some(Credentials { iterations, salt, stored_key, server_key }))
}

/// A number of any BSON numeric type, as a double.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(n) => Some(*n),
        _ => None,
    }
}

/// The `roles` of a stored user as the reference server 9.0.4 reads them (measured): an array of
/// `{role, db}` documents, which may have more fields.
pub fn parse_roles(d: &Document) -> Result<Vec<RoleName>, BadDocument> {
    let roles = match d.get("roles") {
        Some(Value::Array(a)) => a,
        Some(_) => return Err(BadDocument::new(BAD_VALUE, "Invalid roles field, expected array")),
        None => {
            return Err(BadDocument::new(
                UNSUPPORTED_FORMAT,
                "User document needs 'roles' field to be an array",
            ));
        }
    };
    let mut out = Vec::new();
    for r in roles {
        let name = RoleName::parse_stored(r)?;
        if !matches!(r, Value::Document(_)) {
            return Err(BadDocument::new(
                UNSUPPORTED_FORMAT,
                "User document needs values in 'roles' array to be a sub-documents",
            ));
        }
        out.push(name);
    }
    Ok(out)
}

/// The fields of an entry of `authenticationRestrictions`.
const RESTRICTION_FIELDS: [&str; 2] = ["clientSource", "serverAddress"];

/// Each restriction of a stored user is a document of the fields the reference server knows, each
/// an array (measured).
fn check_restrictions(list: &[Value]) -> Result<(), BadDocument> {
    for r in list {
        let Value::Document(r) = r else {
            return Err(BadDocument::new(
                UNSUPPORTED_FORMAT,
                "restriction array sub-documents must be address restriction objects",
            ));
        };
        for (k, v) in r.iter() {
            if !RESTRICTION_FIELDS.contains(&k) {
                return Err(BadDocument::new(
                    BAD_VALUE,
                    format!("BSON field 'address restriction.{k}' is an unknown field."),
                ));
            }
            if !matches!(v, Value::Array(_)) {
                return Err(BadDocument::new(
                    BAD_VALUE,
                    format!(
                        "BSON field 'address restriction.{k}' is the wrong type '{}', expected type 'array'",
                        teseladb_mql::types::name_of(v.element_type())
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// The mechanisms `hello` lists in `saslSupportedMechs` for `name`: the X.509 mechanism for a user
/// of `$external`; the SCRAM mechanisms the user has credentials for; both for a user that does not
/// exist, so the answer does not say which users exist; of those, only the ones `enabled` takes
/// (`authenticationMechanisms`, empty for the default). The reference server 9.0.4 gives them in an
/// order that changes from one process to the next (measured on three restarts of a reference
/// router); TeselaDB gives the order of the stored credentials.
pub fn advertised_mechanisms(
    name: &UserName,
    user: Option<&User>,
    enabled: &[crate::sasl::SaslMechanism],
) -> Vec<&'static str> {
    use crate::sasl::SaslMechanism;
    let on = |m: SaslMechanism| m.is_enabled(enabled);
    if name.db == EXTERNAL_DB {
        return [SaslMechanism::X509].into_iter().filter(|m| on(*m)).map(|m| m.name()).collect();
    }
    let both = [SaslMechanism::ScramSha1, SaslMechanism::ScramSha256];
    let has = |m: SaslMechanism| match (user, m.scram()) {
        (Some(u), Some(scram)) => u.credentials(scram).is_some(),
        _ => true,
    };
    both.into_iter().filter(|m| on(*m) && has(*m)).map(|m| m.name()).collect()
}
