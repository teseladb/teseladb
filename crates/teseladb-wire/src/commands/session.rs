//! `endSessions`, `refreshSessions`, `killSessions`, `killAllSessions` and `startSession`
//! (FR-016): the node's sessions and the cursors that belong to them.

use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;

use super::args::{missing, uuid_value};
use super::{Backend, Ctx, Shared, lock, new_uuid};
use crate::LOGICAL_SESSION_TIMEOUT_MINUTES;
use crate::error::CommandError;
use crate::types::wrong_type;

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
        match d.get("id") {
            Some(Value::Binary { subtype: 4, bytes }) if bytes.len() == 16 => {
                let mut b = [0u8; 16];
                b.copy_from_slice(bytes);
                out.push(u128::from_be_bytes(b));
            }
            Some(v) => return Err(wrong_type(&format!("{list}.id"), v, "binData")),
            None => return Err(missing(&list, "id")),
        }
    }
    Ok(out)
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// Which sessions a kill reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillScope {
    /// These sessions (`killSessions`).
    Sessions(Vec<u128>),
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
        KillScope::Sessions(ids) => {
            g.sessions.end(ids);
            g.cursors.kill_sessions(ids);
        }
        KillScope::All => {
            g.sessions.end_all();
            g.cursors.kill_all_sessions();
        }
        KillScope::Op { op, .. } => {
            g.ops.kill(*op);
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
    let ids = lsids(ctx)?;
    lock(ctx.shared).sessions.mark_ending(&ids);
    Ok(ok())
}

/// `killSessions`.
pub fn kill_sessions<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ids = lsids(ctx)?;
    kill(ctx, KillScope::Sessions(ids))
}

/// `refreshSessions`.
pub fn refresh_sessions<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let ids = lsids(ctx)?;
    let now = ctx.backend.now_ms();
    let mut g = lock(ctx.shared);
    for id in ids {
        g.sessions.touch(id, now);
    }
    Ok(ok())
}

/// `killAllSessions`: every session and its cursors (without users until 013, the list of users
/// it names covers everyone).
pub fn kill_all_sessions<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let list = "KillAllSessionsCmd.killAllSessions";
    let Value::Array(users) = ctx.first().clone() else {
        return Err(wrong_type(list, ctx.first(), "array"));
    };
    for (i, u) in users.iter().enumerate() {
        let Value::Document(d) = u else {
            return Err(wrong_type(&format!("{list}.{i}"), u, "object"));
        };
        for field in ["db", "user"] {
            match d.get(field) {
                Some(Value::String(_)) => {}
                Some(v) => return Err(wrong_type(&format!("{list}.{field}"), v, "string")),
                None => return Err(missing(list, field)),
            }
        }
    }
    // Without authentication (013) every session belongs to no user: a list of users reaches none.
    match users.is_empty() {
        true => kill(ctx, KillScope::All),
        false => Ok(ok()),
    }
}

/// `startSession`: a new session id.
pub fn start_session<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let id = u128::from_be_bytes(new_uuid(ctx.backend));
    let now = ctx.backend.now_ms();
    lock(ctx.shared).sessions.touch(id, now);
    Ok(Document::new()
        .with("id", Document::new().with("id", uuid_value(id)))
        .with("timeoutMinutes", LOGICAL_SESSION_TIMEOUT_MINUTES)
        .with("ok", 1.0))
}

/// What one pattern of `killAllSessionsByPattern` reaches.
enum Pattern {
    Every,
    One(u128),
    Nothing,
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

/// One pattern: its fields must all match a session. Until 013 every session belongs to no user,
/// with the `uid` of the empty name, so a list of users or roles reaches none.
fn pattern(d: &Document, list: &str) -> Result<Pattern, CommandError> {
    let mut out = Pattern::Every;
    for (k, v) in d.iter() {
        let reaches = match (k, v) {
            ("lsid", Value::Document(l)) => {
                let id = match l.get("id") {
                    Some(id) => session_id(id, &format!("{list}.lsid.id"))?,
                    None => return Err(missing(&format!("{list}.lsid"), "id")),
                };
                // The whole session id: `uid` too (measured on a `refrouter` 9.0.4 on 03-11-2026).
                match l.get("uid") {
                    Some(Value::Binary { .. }) => {}
                    Some(other) => {
                        return Err(wrong_type(&format!("{list}.lsid.uid"), other, "binData"));
                    }
                    None => return Err(missing(&format!("{list}.lsid"), "uid")),
                }
                Pattern::One(id)
            }
            ("lsid", other) => return Err(wrong_type(&format!("{list}.lsid"), other, "object")),
            ("uid", Value::Binary { bytes, .. }) => match bytes.as_slice() == sha256(b"") {
                true => Pattern::Every,
                false => Pattern::Nothing,
            },
            ("uid", other) => return Err(wrong_type(&format!("{list}.uid"), other, "binData")),
            ("users" | "roles", Value::Array(a)) => {
                let name = if k == "users" { "user" } else { "role" };
                for (i, e) in a.iter().enumerate() {
                    named(e, &format!("{list}.{k}"), i, name)?;
                }
                match a.is_empty() {
                    true => Pattern::Every,
                    false => Pattern::Nothing,
                }
            }
            ("users" | "roles", other) => {
                return Err(wrong_type(&format!("{list}.{k}"), other, "array"));
            }
            _ => {
                let msg = format!("BSON field '{list}.{k}' is an unknown field.");
                return Err(CommandError::new(crate::error::IDL_UNKNOWN_FIELD, msg));
            }
        };
        out = match (out, reaches) {
            (Pattern::Nothing, _) | (_, Pattern::Nothing) => Pattern::Nothing,
            (Pattern::One(a), _) | (_, Pattern::One(a)) => Pattern::One(a),
            (Pattern::Every, Pattern::Every) => Pattern::Every,
        };
    }
    Ok(out)
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
    let (mut every, mut ids) = (patterns.is_empty(), Vec::new());
    for (i, p) in patterns.iter().enumerate() {
        let Value::Document(d) = p else {
            return Err(wrong_type(&format!("{list}.{i}"), p, "object"));
        };
        match pattern(d, list)? {
            Pattern::Every => every = true,
            Pattern::One(id) => ids.push(id),
            Pattern::Nothing => {}
        }
    }
    may_impersonate(ctx.backend, &patterns)?;
    match (every, ids.is_empty()) {
        (true, _) => kill(ctx, KillScope::All),
        (false, false) => kill(ctx, KillScope::Sessions(ids)),
        (false, true) => Ok(ok()),
    }
}
