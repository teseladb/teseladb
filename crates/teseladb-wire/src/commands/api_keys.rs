//! The API keys of the HTTP API (019 FR-016), the commands behind `/v1/apiKeys` and
//! `teseladb admin apikey`, run on `admin`:
//!
//! - `{createApiKey: <user>, db: <its database, admin>, roles: [...], expiresAfterDays: n}`
//!   answers the key once, with what `listApiKeys` shows of it;
//! - `{listApiKeys: 1, user: <u>, db: <d>}` lists the keys (without the key or its hash);
//! - `{revokeApiKey: <keyId>}` removes it, and answers once no node authorizes with it (the fence
//!   of 013 FR-012).
//!
//! Anyone authenticated creates, lists and revokes keys of their own user, as they change their
//! own password; the keys of another user need `createUser`, `viewUser` or `dropUser` on the
//! user's database. A key carries roles the user holds directly, and a key created by a client
//! logged in with a key carries at most that key's roles.

use teseladb_auth::actions::ActionType;
use teseladb_auth::apikey::{self, ApiKey};
use teseladb_auth::authz::{self, Target};
use teseladb_auth::users::{RoleName, UserName};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_bson::{Document, RawDoc, Value};

use super::args;
use super::authz_writes::key_error;
use super::http_parameters::{HTTP_OPTIONS, http_options};
use super::users::{MAX_ATTEMPTS, bad_value, conflict, roles_arg};
use super::{Backend, Ctx};
use crate::error::{CommandError, INTERNAL_ERROR, KEY_NOT_FOUND, UNAUTHORIZED, USER_NOT_FOUND};

/// `createApiKey`.
pub const CREATE_API_KEY: &str = "createApiKey";
/// `listApiKeys`.
pub const LIST_API_KEYS: &str = "listApiKeys";
/// `revokeApiKey`.
pub const REVOKE_API_KEY: &str = "revokeApiKey";
/// The commands of this module.
pub const NAMES: [&str; 3] = [CREATE_API_KEY, LIST_API_KEYS, REVOKE_API_KEY];
/// The database a key's user belongs to when `db` is not given.
pub const DEFAULT_DB: &str = "admin";

/// Runs the command of `ctx`; `None` if it is not one of this module's.
pub fn run<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Option<Result<Document, CommandError>> {
    Some(match ctx.name() {
        CREATE_API_KEY => create(ctx),
        LIST_API_KEYS => list(ctx),
        REVOKE_API_KEY => revoke(ctx),
        _ => return None,
    })
}

/// Whether the running command may act on the keys of `user` with `action`: the user's own, or
/// `action` on its database; everything on a node that does not authorize.
fn may<B: Backend + ?Sized>(backend: &B, user: &UserName, action: ActionType) -> bool {
    let Some(privileges) = backend.privileges() else { return true };
    backend.authenticated_user().as_ref() == Some(user)
        || authz::allows(&privileges, &Target::database(&user.db), action)
}

fn denied(cmd: &str, user: &UserName) -> CommandError {
    CommandError::new(
        UNAUTHORIZED,
        format!("not authorized to run {cmd} for the API keys of {}@{}", user.user, user.db),
    )
}

fn stored(doc: &[u8]) -> Result<ApiKey, CommandError> {
    let d = Document::from(RawDoc::from_bytes(doc)?);
    ApiKey::from_document(&d)
        .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("bad stored API key: {}", e.0)))
}

/// The roles the caller may put in a key of `user`, who holds `held` directly: those, and when
/// the caller is `user` logged in with a key, only the roles of that key.
fn grantable<B: Backend + ?Sized>(
    backend: &B,
    user: &UserName,
    held: Vec<RoleName>,
) -> Vec<RoleName> {
    match backend.authenticated_roles() {
        Some(own) if backend.authenticated_user().as_ref() == Some(user) => {
            held.into_iter().filter(|r| own.contains(r)).collect()
        }
        _ => held,
    }
}

/// `createApiKey`.
fn create<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let cmd = ctx.cmd;
    let Value::String(name) = ctx.first() else {
        return Err(bad_value("createApiKey takes the user's name"));
    };
    let db = args::string(cmd, CREATE_API_KEY, "db")?.unwrap_or(DEFAULT_DB);
    let user = UserName::new(name.as_str(), db);
    if !may(&*ctx.backend, &user, ActionType::CreateUser) {
        return Err(denied(CREATE_API_KEY, &user));
    }
    let Some(found) = ctx.backend.user(&user)? else {
        let msg = format!("User {}@{} not found", user.user, user.db);
        return Err(CommandError::new(USER_NOT_FOUND, msg));
    };
    let allowed = grantable(&*ctx.backend, &user, found.roles);
    let roles = match roles_arg(cmd, CREATE_API_KEY, db)? {
        None => allowed,
        Some(asked) => {
            if let Some(r) = asked.iter().find(|r| !allowed.contains(r)) {
                return Err(bad_value(format!(
                    "the key cannot carry role {}@{}: {}@{} does not hold it",
                    r.role, r.db, user.user, user.db
                )));
            }
            asked
        }
    };
    let days = args::int(cmd, CREATE_API_KEY, "expiresAfterDays")?;
    let stored_options = ctx.backend.cluster_parameters()?.get(HTTP_OPTIONS).cloned();
    let max = http_options(stored_options.as_deref()).api_key_max_days;
    let now = i64::try_from(ctx.backend.wall_ms()).unwrap_or(i64::MAX);
    let expires = apikey::expiry(now, days, max).map_err(bad_value)?;
    let mut random = [0u8; apikey::KEY_BYTES];
    ctx.backend.secure_random(&mut random)?;
    let mut id = [0u8; apikey::KEY_ID_BYTES];
    ctx.backend.secure_random(&mut id)?;
    let key = apikey::encode_key(&random);
    let record = ApiKey {
        hash: apikey::hash_hex(&key),
        key_id: apikey::hex(&id),
        prefix: apikey::shown_prefix(&key),
        user: user.user.clone(),
        db: user.db.clone(),
        roles,
        created_ms: now,
        expires_ms: expires,
        last_used_ms: None,
    };
    let doc = record.to_document().to_bytes()?;
    let op = SecurityOp::put(Principal::ApiKey, Expect::Absent, doc).map_err(key_error)?;
    if ctx.backend.security_write(vec![op])?.is_err() {
        return Err(conflict());
    }
    let mut out = Document::new().with("key", key);
    for (k, v) in record.public_document().iter() {
        out.push(k, v.clone());
    }
    Ok(out.with("ok", 1.0))
}

/// `listApiKeys`: the keys the caller may see, of `user` and `db` when given, in creation order.
fn list<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let cmd = ctx.cmd;
    let user = args::string(cmd, LIST_API_KEYS, "user")?.map(str::to_owned);
    let db = args::string(cmd, LIST_API_KEYS, "db")?.map(str::to_owned);
    let mut keys = Vec::new();
    for doc in ctx.backend.principals(Principal::ApiKey, db.as_deref())? {
        let k = stored(&doc)?;
        if user.as_ref().is_some_and(|u| *u != k.user) {
            continue;
        }
        if may(&*ctx.backend, &UserName::new(k.user.as_str(), k.db.as_str()), ActionType::ViewUser)
        {
            keys.push(Value::Document(k.public_document()));
        }
    }
    Ok(Document::new().with("keys", Value::Array(keys)).with("ok", 1.0))
}

/// `revokeApiKey`.
fn revoke<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let Value::String(key_id) = ctx.first() else {
        return Err(bad_value("revokeApiKey takes the key's keyId"));
    };
    let key_id = key_id.clone();
    for _ in 0..MAX_ATTEMPTS {
        let mut found = None;
        for doc in ctx.backend.principals(Principal::ApiKey, None)? {
            if stored(&doc)?.key_id == key_id {
                found = Some(doc);
                break;
            }
        }
        let Some(doc) = found else {
            return Err(CommandError::new(KEY_NOT_FOUND, format!("no API key {key_id}")));
        };
        let k = stored(&doc)?;
        let user = UserName::new(k.user.as_str(), k.db.as_str());
        if !may(&*ctx.backend, &user, ActionType::DropUser) {
            return Err(denied(REVOKE_API_KEY, &user));
        }
        let op = SecurityOp::remove(Principal::ApiKey, doc).map_err(key_error)?;
        if ctx.backend.security_write(vec![op])?.is_ok() {
            return Ok(Document::new().with("ok", 1.0));
        }
    }
    Err(conflict())
}

/// Moves `lastUsedAt` of `key` to now if it is due ([`ApiKey::due_touch`]); a concurrent change
/// of the key (a revocation) wins.
pub fn touch<B: Backend + ?Sized>(backend: &mut B, key: &ApiKey) -> Result<(), CommandError> {
    let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
    if !key.due_touch(now) {
        return Ok(());
    }
    let old = key.to_document().to_bytes()?;
    let new = ApiKey { last_used_ms: Some(now), ..key.clone() }.to_document().to_bytes()?;
    let op = SecurityOp::put(Principal::ApiKey, Expect::Exactly(old), new).map_err(key_error)?;
    let _ = backend.security_write(vec![op])?;
    Ok(())
}
