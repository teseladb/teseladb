//! API keys of the HTTP API (019 FR-016, `data-model.md`): `tdb_` and 32 random bytes in
//! base64url without padding, shown once. Group 0 keeps only the SHA-256 of the key, as the
//! `_id` of its document, with its `keyId` (16 random bytes in hexadecimal), the eight characters
//! after `tdb_` that identify it, its user, the roles it carries (a subset of the user's), its
//! creation, its expiry and its last use. A key authenticates its user with the roles that the
//! user still holds among the key's (FR-017).

use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;
use teseladb_x509::pem::base64_encode;

use crate::users::RoleName;

/// What every key starts with.
pub const KEY_PREFIX: &str = "tdb_";
/// Random bytes of a key.
pub const KEY_BYTES: usize = 32;
/// Characters of the random part: 32 bytes in base64url without padding.
pub const ENCODED_LEN: usize = 43;
/// Characters after `tdb_` that `list` shows to identify a key.
pub const SHOWN_LEN: usize = 8;
/// Random bytes of a `keyId`.
pub const KEY_ID_BYTES: usize = 16;
/// Days a key lives when `expiresAfterDays` is not given.
pub const DEFAULT_EXPIRES_DAYS: u64 = 90;
/// The longest life of a key unless `httpOptions.apiKeys.maxExpiresAfterDays` says otherwise.
pub const DEFAULT_MAX_EXPIRES_DAYS: u64 = 365;
/// A key's `lastUsedAt` moves at most once in this many milliseconds.
pub const TOUCH_EVERY_MS: i64 = 60_000;
/// Milliseconds of a day.
pub const DAY_MS: i64 = 86_400_000;
/// The name the audit log and `connectionStatus` give the way a key logs in.
pub const MECHANISM: &str = "TESELADB-APIKEY";

/// The key whose random part is `random`.
#[must_use]
pub fn encode_key(random: &[u8; KEY_BYTES]) -> String {
    let b64: String = base64_encode(random)
        .chars()
        .filter(|c| *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    format!("{KEY_PREFIX}{b64}")
}

/// Whether `token` has the shape of a key: `tdb_` and 43 characters of base64url.
#[must_use]
pub fn is_key(token: &str) -> bool {
    token.strip_prefix(KEY_PREFIX).is_some_and(|rest| {
        rest.len() == ENCODED_LEN
            && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// The SHA-256 of `key` in lowercase hexadecimal: the `_id` of its document.
#[must_use]
pub fn hash_hex(key: &str) -> String {
    hex(&sha256(key.as_bytes()))
}

/// `bytes` in lowercase hexadecimal.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// What identifies `key` in a listing: `tdb_` and the next eight characters.
#[must_use]
pub fn shown_prefix(key: &str) -> String {
    key.chars().take(KEY_PREFIX.len() + SHOWN_LEN).collect()
}

/// A key as Group 0 keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKey {
    /// The SHA-256 of the key, hexadecimal.
    pub hash: String,
    /// Its identifier, hexadecimal.
    pub key_id: String,
    /// [`shown_prefix`].
    pub prefix: String,
    /// The user it logs in as.
    pub user: String,
    /// The user's database.
    pub db: String,
    /// The roles it carries.
    pub roles: Vec<RoleName>,
    /// When it was created, in milliseconds since the epoch.
    pub created_ms: i64,
    /// When it stops working.
    pub expires_ms: i64,
    /// When it was last used, if ever.
    pub last_used_ms: Option<i64>,
}

/// Why a stored document is not a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadKeyDocument(pub &'static str);

fn string(d: &Document, f: &'static str) -> Result<String, BadKeyDocument> {
    match d.get(f) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(BadKeyDocument(f)),
    }
}

fn date(d: &Document, f: &'static str) -> Result<Option<i64>, BadKeyDocument> {
    match d.get(f) {
        Some(Value::DateTime(ms)) => Ok(Some(*ms)),
        None | Some(Value::Null) => Ok(None),
        _ => Err(BadKeyDocument(f)),
    }
}

impl ApiKey {
    /// Its document in Group 0.
    #[must_use]
    pub fn to_document(&self) -> Document {
        let mut d = Document::new().with("_id", self.hash.as_str());
        for (k, v) in self.public_document().iter() {
            d.push(k, v.clone());
        }
        d
    }

    /// What `list` shows of it: everything but the hash.
    #[must_use]
    pub fn public_document(&self) -> Document {
        let roles: Vec<Value> = self.roles.iter().map(RoleName::to_value).collect();
        Document::new()
            .with("keyId", self.key_id.as_str())
            .with("prefix", self.prefix.as_str())
            .with("user", self.user.as_str())
            .with("db", self.db.as_str())
            .with("roles", Value::Array(roles))
            .with("createdAt", Value::DateTime(self.created_ms))
            .with("expiresAt", Value::DateTime(self.expires_ms))
            .with("lastUsedAt", self.last_used_ms.map_or(Value::Null, Value::DateTime))
    }

    /// Reads what [`ApiKey::to_document`] wrote.
    pub fn from_document(d: &Document) -> Result<ApiKey, BadKeyDocument> {
        let roles = match d.get("roles") {
            Some(Value::Array(list)) => list
                .iter()
                .map(|v| RoleName::from_value(v).ok_or(BadKeyDocument("roles")))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(BadKeyDocument("roles")),
        };
        Ok(ApiKey {
            hash: string(d, "_id")?,
            key_id: string(d, "keyId")?,
            prefix: string(d, "prefix")?,
            user: string(d, "user")?,
            db: string(d, "db")?,
            roles,
            created_ms: date(d, "createdAt")?.ok_or(BadKeyDocument("createdAt"))?,
            expires_ms: date(d, "expiresAt")?.ok_or(BadKeyDocument("expiresAt"))?,
            last_used_ms: date(d, "lastUsedAt")?,
        })
    }

    /// Whether it no longer works at `now_ms`.
    #[must_use]
    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_ms
    }

    /// Whether its `lastUsedAt` is due to move at `now_ms` ([`TOUCH_EVERY_MS`]).
    #[must_use]
    pub fn due_touch(&self, now_ms: i64) -> bool {
        self.last_used_ms.is_none_or(|t| now_ms - t >= TOUCH_EVERY_MS)
    }

    /// The roles it logs in with: those of the key the user still holds directly (FR-017).
    #[must_use]
    pub fn effective_roles(&self, user_roles: &[RoleName]) -> Vec<RoleName> {
        self.roles.iter().filter(|r| user_roles.contains(r)).cloned().collect()
    }
}

/// When a key created at `now_ms` for `days` days expires; `days` must be within `1..=max`.
pub fn expiry(now_ms: i64, days: Option<i64>, max: u64) -> Result<i64, String> {
    let max = i64::try_from(max).unwrap_or(i64::MAX);
    let days = days.unwrap_or(DEFAULT_EXPIRES_DAYS.cast_signed().min(max));
    if days < 1 || days > max {
        return Err(format!("expiresAfterDays must be between 1 and {max}"));
    }
    Ok(now_ms.saturating_add(days.saturating_mul(DAY_MS)))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn key() -> ApiKey {
        ApiKey {
            hash: hash_hex("tdb_x"),
            key_id: hex(&[1; KEY_ID_BYTES]),
            prefix: "tdb_abcdefgh".into(),
            user: "ann".into(),
            db: "admin".into(),
            roles: vec![RoleName::new("read", "app"), RoleName::new("readWrite", "app")],
            created_ms: 1_000,
            expires_ms: 1_000 + DAY_MS,
            last_used_ms: None,
        }
    }

    #[test]
    fn a_key_is_tdb_and_43_characters_of_base64url() {
        let k = encode_key(&[0xfb; KEY_BYTES]);
        assert!(k.starts_with(KEY_PREFIX));
        assert_eq!(k.len(), KEY_PREFIX.len() + ENCODED_LEN);
        assert!(!k.contains('+') && !k.contains('/') && !k.contains('='));
        assert!(k.contains('-') || k.contains('_'));
        assert!(is_key(&k));
        assert!(!is_key("tdb_short"));
        assert!(!is_key(&k.replace("tdb_", "tdc_")));
        assert!(!is_key(&format!("{}!", &k[..k.len() - 1])));
        assert_eq!(shown_prefix(&k).len(), 12);
        assert_eq!(hash_hex(&k).len(), 64);
        assert_eq!(hex(&[0, 255]), "00ff");
    }

    #[test]
    fn the_document_reads_back_and_never_shows_the_hash_in_a_listing() {
        let mut k = key();
        assert_eq!(ApiKey::from_document(&k.to_document()), Ok(k.clone()));
        k.last_used_ms = Some(5);
        assert_eq!(ApiKey::from_document(&k.to_document()), Ok(k.clone()));
        assert!(k.public_document().get("_id").is_none());
        let mut bad = k.to_document();
        bad.insert("roles", 1);
        assert_eq!(ApiKey::from_document(&bad), Err(BadKeyDocument("roles")));
        let mut bad = k.to_document();
        bad.insert("roles", Value::Array(vec![Value::Int32(1)]));
        assert_eq!(ApiKey::from_document(&bad), Err(BadKeyDocument("roles")));
        let mut bad = k.to_document();
        bad.insert("createdAt", "x");
        assert_eq!(ApiKey::from_document(&bad), Err(BadKeyDocument("createdAt")));
        let mut bad = k.to_document();
        bad.remove("expiresAt");
        assert_eq!(ApiKey::from_document(&bad), Err(BadKeyDocument("expiresAt")));
        let mut bad = k.to_document();
        bad.remove("user");
        assert_eq!(ApiKey::from_document(&bad), Err(BadKeyDocument("user")));
    }

    #[test]
    fn expiry_use_and_roles() {
        let k = key();
        assert!(!k.expired(1_000));
        assert!(k.expired(1_000 + DAY_MS));
        assert!(k.due_touch(0));
        let used = ApiKey { last_used_ms: Some(0), ..key() };
        assert!(!used.due_touch(TOUCH_EVERY_MS - 1));
        assert!(used.due_touch(TOUCH_EVERY_MS));
        let held = [RoleName::new("read", "app")];
        assert_eq!(k.effective_roles(&held), held.to_vec());
        assert_eq!(expiry(0, None, 365), Ok(90 * DAY_MS));
        assert_eq!(expiry(0, None, 30), Ok(30 * DAY_MS));
        assert_eq!(expiry(0, Some(2), 365), Ok(2 * DAY_MS));
        assert!(expiry(0, Some(0), 365).is_err());
        assert!(expiry(0, Some(366), 365).is_err());
    }
}
