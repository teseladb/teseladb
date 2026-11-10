//! `endSessions`, `refreshSessions`, `killSessions`, `killAllSessions` and `startSession`
//! (FR-016): the node's sessions and the cursors that belong to them.

pub use teseladb_session::session::{Owner, SessionMatch, Uid};
use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;

use super::args::{missing, uuid_value};
use super::{Backend, Ctx, Shared, lock, new_uuid};
use crate::LOGICAL_SESSION_TIMEOUT_MINUTES;
use crate::error::CommandError;
use crate::types::wrong_type;

/// The `uid` of user `user@db` (`name`), the empty name without authentication.
pub fn uid_of(name: &str) -> Uid {
    sha256(name.as_bytes())
}

/// Who runs the command: the owner of the sessions it starts.
pub fn caller<B: Backend + ?Sized>(backend: &B) -> Owner {
    match backend.authenticated_user() {
        Some(u) => {
            let name = format!("{}@{}", u.user, u.db);
            Owner { uid: uid_of(&name), user: Some(name) }
        }
        None => Owner { uid: uid_of(""), user: None },
    }
}

/// The owner's `uid` of a cursor opened by `owner` (`db.user`, `None` without authentication).
fn cursor_uid(owner: Option<&str>) -> Uid {
    match owner.and_then(|o| o.split_once('.')) {
        Some((db, user)) => uid_of(&format!("{user}@{db}")),
        None => uid_of(""),
    }
}

/// A `uid` at `path`: a general binary of 32 bytes (measured on a `refrouter` 9.0.4).
fn uid_value(v: &Value, path: &str) -> Result<Uid, CommandError> {
    match v {
        Value::Binary { subtype: 0, bytes } => <Uid>::try_from(bytes.as_slice()).map_err(|_| {
            let msg = format!("Unsupported SHA256Block hash length: {}", bytes.len());
            CommandError::new(crate::error::UNSUPPORTED_FORMAT, msg)
        }),
        Value::Binary { subtype, .. } => {
            let name = crate::cluster_time::subtype_name(*subtype);
            let msg = format!(
                "BSON field '{path}' is the wrong binData type '{name}', expected type 'general'"
            );
            Err(CommandError::new(crate::error::TYPE_MISMATCH, msg))
        }
        other => Err(wrong_type(path, other, "binData")),
    }
}

/// `endSessions`.
pub const END_SESSIONS: &str = "endSessions";
/// `refreshSessions`.
pub const REFRESH_SESSIONS: &str = "refreshSessions";
/// `killSessions`.
pub const KILL_SESSIONS: &str = "killSessions";
/// `killAllSessions`.
pub const KILL_ALL_SESSIONS: &str = "killAllSessions";
/// `startSession`.
pub const START_SESSION: &str = "startSession";
/// `killAllSessionsByPattern`.
pub const KILL_ALL_SESSIONS_BY_PATTERN: &str = "killAllSessionsByPattern";

/// How RefDb 9.0.4 names the session list of `command` in its messages (measured).
fn list_name(command: &str) -> String {
    match command {
        KILL_SESSIONS => format!("KillSessionsCmd.{command}"),
        other => format!("{other}.{other}FromClient"),
    }
}

/// The session ids of the command's first field (an array of `{id: UUID}`).
fn lsids<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Result<Vec<u128>, CommandError> {
    Ok(named_sessions(ctx)?.into_iter().map(|(id, _)| id).collect())
}

/// The sessions of the command's first field, each of the `uid` its `lsid` names or else of
/// the user who runs the command, as RefDb completes an `lsid` (measured on a `refrouter` 9.0.4
/// with authentication on 03-11-2026: `killSessions` of another user's `id` reaches nothing).
fn matches<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Result<Vec<SessionMatch>, CommandError> {
    let own = caller(ctx.backend).uid;
    Ok(named_sessions(ctx)?
        .into_iter()
        .map(|(id, uid)| SessionMatch::one(id, uid.unwrap_or(own)))
        .collect())
}

/// The `id` and the `uid`, if it has one, of each element of the command's first field.
fn named_sessions<B: Backend + ?Sized>(
    ctx: &Ctx<'_, B>,
) -> Result<Vec<(u128, Option<Uid>)>, CommandError> {
    let name = ctx.name().to_owned();
    let list = list_name(&name);
    let Value::Array(items) = ctx.first() else {
        let whole = match name.as_str() {
            KILL_SESSIONS => list,
            _ => format!("{name}.{name}"),
        };
        return Err(wrong_type(&whole, ctx.first(), "array"));
    };
    let mut out = Vec::new();
    for (i, v) in items.iter().enumerate() {
        let Value::Document(d) = v else {
            return Err(wrong_type(&format!("{list}.{i}"), v, "object"));
        };
        let id = match d.get("id") {
            Some(Value::Binary { subtype: 4, bytes }) if bytes.len() == 16 => {
                let mut b = [0u8; 16];
                b.copy_from_slice(bytes);
                u128::from_be_bytes(b)
            }
            Some(v) => return Err(wrong_type(&format!("{list}.id"), v, "binData")),
            None => return Err(missing(&list, "id")),
        };
        let uid = d.get("uid").map(|u| uid_value(u, &format!("{list}.uid"))).transpose()?;
        out.push((id, uid));
    }
    Ok(out)
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// Which sessions a kill reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillScope {
    /// The sessions these reach, by their owners (`killSessions`, `killAllSessions` with users,
    /// `killAllSessionsByPattern`).
    Sessions(Vec<SessionMatch>),
    /// Every session (`killAllSessions`).
    All,
    /// One operation of one node (`killOp` of `node<N>:<op>`, 010 FR-016): only that node
    /// gets it.
    Op {
        /// The node.
        node: u64,
        /// The operation's number in the node's table.
        op: u32,
    },
}

/// Ends the sessions of `scope` on this node and kills their cursors, or marks its operation.
pub fn kill_local(shared: &Shared, scope: &KillScope) {
    let mut g = lock(shared);
    match scope {
        KillScope::Sessions(matches) => {
            g.sessions.end_matching(matches);
            g.cursors.kill_matching(matches, |c| cursor_uid(c.owner.as_deref()));
        }
        KillScope::All => {
            g.sessions.end_all();
            g.cursors.kill_all_sessions();
        }
        // With the node's lock held: `hot::of` could take it again.
        KillScope::Op { op, .. } => {
            g.hot.ops.kill(*op);
        }
    }
}

/// Kills the sessions of `scope` on this node and then on every other node of the cluster, as a
/// `refrouter` sends `killSessions` to every shard.
fn kill<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    scope: KillScope,
) -> Result<Document, CommandError> {
    kill_local(ctx.shared, &scope);
    ctx.backend.kill_everywhere(&scope)?;
    Ok(ok())
}

/// `endSessions`: the sessions end, with their cursors, at the node's next refresh of the
/// sessions' record (`registry::refresh`), as RefDb ends them at its session cache's refresh.
pub fn end_sessions<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ending = matches(ctx)?;
    lock(ctx.shared).sessions.mark_ending(&ending);
    Ok(ok())
}

/// `killSessions`.
pub fn kill_sessions<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let reached = matches(ctx)?;
    kill(ctx, KillScope::Sessions(reached))
}

/// `refreshSessions`.
pub fn refresh_sessions<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let ids = lsids(ctx)?;
    let now = ctx.backend.now_ms();
    let owner = caller(ctx.backend);
    let mut g = lock(ctx.shared);
    for id in ids {
        g.sessions.touch(id, &owner, now);
    }
    Ok(ok())
}

/// `killAllSessions`: the sessions of the users it names, with their cursors; every session with
/// no user named.
pub fn kill_all_sessions<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let list = "KillAllSessionsCmd.killAllSessions";
    let Value::Array(users) = ctx.first().clone() else {
        return Err(wrong_type(list, ctx.first(), "array"));
    };
    let mut owners = Vec::new();
    for (i, u) in users.iter().enumerate() {
        let Value::Document(d) = u else {
            return Err(wrong_type(&format!("{list}.{i}"), u, "object"));
        };
        let mut name = [""; 2];
        for (slot, field) in name.iter_mut().zip(["db", "user"]) {
            match d.get(field) {
                Some(Value::String(v)) => *slot = v,
                Some(v) => return Err(wrong_type(&format!("{list}.{field}"), v, "string")),
                None => return Err(missing(list, field)),
            }
        }
        let [db, user] = name;
        owners.push(SessionMatch::owner(uid_of(&format!("{user}@{db}"))));
    }
    match owners.is_empty() {
        true => kill(ctx, KillScope::All),
        false => kill(ctx, KillScope::Sessions(owners)),
    }
}

/// `startSession`: a new session id.
pub fn start_session<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let id = u128::from_be_bytes(new_uuid(ctx.backend));
    let now = ctx.backend.now_ms();
    let owner = caller(ctx.backend);
    lock(ctx.shared).sessions.touch(id, &owner, now);
    Ok(Document::new()
        .with("id", Document::new().with("id", uuid_value(id)))
        .with("timeoutMinutes", LOGICAL_SESSION_TIMEOUT_MINUTES)
        .with("ok", 1.0))
}

fn session_id(v: &Value, path: &str) -> Result<u128, CommandError> {
    match v {
        Value::Binary { subtype: 4, bytes } if bytes.len() == 16 => {
            let mut b = [0u8; 16];
            b.copy_from_slice(bytes);
            Ok(u128::from_be_bytes(b))
        }
        other => Err(wrong_type(path, other, "binData")),
    }
}

/// The sessions one pattern reaches, `None` for every session. A `uid` reaches every session
/// of its owner, also next to an `lsid`; an `lsid` reaches the session with its `id` and `uid`;
/// `users` and `roles` name whom the command acts for and choose no session (measured on a
/// `refrouter` 9.0.4 with authentication on 03-11-2026).
fn pattern(d: &Document, list: &str) -> Result<Option<SessionMatch>, CommandError> {
    let (mut uid, mut one) = (None, None);
    for (k, v) in d.iter() {
        match (k, v) {
            ("lsid", Value::Document(l)) => {
                let id = match l.get("id") {
                    Some(id) => session_id(id, &format!("{list}.lsid.id"))?,
                    None => return Err(missing(&format!("{list}.lsid"), "id")),
                };
                let path = format!("{list}.lsid.uid");
                match l.get("uid") {
                    Some(u) => one = Some(SessionMatch::one(id, uid_value(u, &path)?)),
                    None => return Err(missing(&format!("{list}.lsid"), "uid")),
                }
            }
            ("lsid", other) => return Err(wrong_type(&format!("{list}.lsid"), other, "object")),
            ("uid", v) => uid = Some(SessionMatch::owner(uid_value(v, &format!("{list}.uid"))?)),
            ("users" | "roles", Value::Array(a)) => {
                let name = if k == "users" { "user" } else { "role" };
                for (i, e) in a.iter().enumerate() {
                    named(e, &format!("{list}.{k}"), i, name)?;
                }
            }
            ("users" | "roles", other) => {
                return Err(wrong_type(&format!("{list}.{k}"), other, "array"));
            }
            _ => {
                let msg = format!("BSON field '{list}.{k}' is an unknown field.");
                return Err(CommandError::new(crate::error::IDL_UNKNOWN_FIELD, msg));
            }
        }
    }
    Ok(uid.or(one))
}

/// One user (`name` is `user`) or role (`role`) of a pattern, element `i` of the list at `path`:
/// a document with a string `name` and `db`, nothing else (measured on a `refrouter` 9.0.4 on
/// 03-11-2026; a missing `db` is named before a missing `name`).
fn named(e: &Value, path: &str, i: usize, name: &str) -> Result<(), CommandError> {
    let Value::Document(d) = e else {
        return Err(wrong_type(&format!("{path}.{i}"), e, "object"));
    };
    for (k, v) in d.iter() {
        match (k, v) {
            (k, Value::String(_)) if k == name || k == "db" => {}
            (k, v) if k == name || k == "db" => {
                return Err(wrong_type(&format!("{path}.{k}"), v, "string"));
            }
            (k, _) => {
                let msg = format!("BSON field '{path}.{k}' is an unknown field.");
                return Err(CommandError::new(crate::error::IDL_UNKNOWN_FIELD, msg));
            }
        }
    }
    for field in ["db", name] {
        if d.get(field).is_none() {
            return Err(missing(path, field));
        }
    }
    Ok(())
}

/// A pattern with `users` or `roles` asks for `impersonate` on the cluster, which only
/// `__system` has; a node that does not authorize asks nothing (measured on a `refrouter` 9.0.4
/// with `--keyFile` and a `refnode` without `--auth` on 03-11-2026).
fn may_impersonate<B: Backend + ?Sized>(
    backend: &B,
    patterns: &[Value],
) -> Result<(), CommandError> {
    let names = patterns.iter().any(
        |p| matches!(p, Value::Document(d) if d.get("users").is_some() || d.get("roles").is_some()),
    );
    let Some(privileges) = backend.privileges() else { return Ok(()) };
    let allowed = teseladb_auth::authz::allows(
        &privileges,
        &teseladb_auth::authz::Target::Cluster,
        teseladb_auth::actions::ActionType::Impersonate,
    );
    match names && !allowed {
        true => Err(CommandError::new(
            crate::error::UNAUTHORIZED,
            "Not authorized to impersonate in killAllSessionsByPattern",
        )),
        false => Ok(()),
    }
}

/// `killAllSessionsByPattern`: the sessions any of the patterns reaches, with their cursors;
/// an empty list reaches every session.
pub fn kill_all_sessions_by_pattern<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let list = "KillAllSessionsByPatternCmd.killAllSessionsByPattern";
    let Value::Array(patterns) = ctx.first().clone() else {
        return Err(wrong_type(list, ctx.first(), "array"));
    };
    let (mut every, mut reached) = (patterns.is_empty(), Vec::new());
    for (i, p) in patterns.iter().enumerate() {
        let Value::Document(d) = p else {
            return Err(wrong_type(&format!("{list}.{i}"), p, "object"));
        };
        match pattern(d, list)? {
            None => every = true,
            Some(m) => reached.push(m),
        }
    }
    may_impersonate(ctx.backend, &patterns)?;
    match every {
        true => kill(ctx, KillScope::All),
        false => kill(ctx, KillScope::Sessions(reached)),
    }
}
