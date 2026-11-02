//! Authorization of each command (013 FR-011, T023): the user's privileges from its roles in
//! Group 0, the table of `teseladb_auth::authz_rules`, the cursors of the node for `getMore` and
//! `killCursors`, and RefDb 9.0.4's `Unauthorized` messages (measured on a `refrouter`).

use teseladb_auth::actions::ActionType;
use teseladb_auth::authz::{self, Check, Target};
use teseladb_auth::authz_rules::{self, Subject};
use teseladb_auth::privileges::Privilege;
use teseladb_bson::{Document, Value};

use super::roles::Graph;
use super::{Backend, Shared, display, lock};
use crate::connection::{Connection, Request};
use crate::error::{CommandError, NOT_PRIMARY_OR_SECONDARY, UNAUTHORIZED};

/// Bytes from which RefDb shortens a string it quotes in a message (measured: 159 stay whole,
/// 160 are cut).
pub const QUOTE_CUT_FROM: usize = 160;
/// Bytes a shortened string keeps, before `...`.
pub const QUOTE_KEEP: usize = 150;
/// What RefDb writes for a password it quotes.
pub const REDACTED: &str = "xxx";
/// The commands whose `pwd` RefDb hides in its messages.
const WITH_PASSWORD: [&str; 2] = ["createUser", "updateUser"];
/// The answer of a node without an authorization lease: drivers take the code as a node that is
/// recovering and send the command to another (D-073).
pub const LEASE_LOST: &str = "node is recovering: its copy of the users and roles may be behind and it authorizes nothing until it catches up";
/// The field RefDb hides.
const PASSWORD_FIELD: &str = "pwd";

/// Authorizes `req` on `conn` and returns the user's privileges, or `None` when the node does not
/// authorize (`--insecure`, as a RefDb without `--auth`). Also sets whether the connection may
/// skip the default time limit (`bypassDefaultMaxTimeMS` on the cluster).
pub fn authorize<B: Backend + ?Sized>(
    conn: &mut Connection,
    req: &Request,
    name: &str,
    backend: &mut B,
    shared: &Shared,
) -> Result<Option<Vec<Privilege>>, CommandError> {
    if !conn.auth.required {
        return Ok(None);
    }
    if !backend.authorization_lease() {
        return Err(CommandError::new(NOT_PRIMARY_OR_SECONDARY, LEASE_LOST));
    }
    let Some(auth) = conn.auth.user.clone() else {
        // Only the commands open before authentication get here.
        return Ok(Some(Vec::new()));
    };
    let who = auth.name.clone();
    let roles = auth.direct_roles(&mut *backend)?;
    let mut privileges = Graph::new(&mut *backend).inherit(&roles, None)?.privileges;
    // What the tenants share is the default tenant's to administer (013 FR-024).
    if auth.tenant != teseladb_meta::tenants::DEFAULT_TENANT {
        privileges = teseladb_auth::privileges::of_tenant(privileges);
    }
    conn.bypass_default_max_time =
        authz::allows(&privileges, &Target::Cluster, ActionType::BypassDefaultMaxTimeMS);
    let cmd = by_name(backend, &req.db, &req.command)?;
    let subject = Subject { user: &who, roles: &roles };
    let check = authz_rules::check(name, &req.db, &cmd, &subject);
    let owner = who.to_string();
    match &check {
        Check::GetMore { id, .. } => {
            let found = lock(shared).cursors.get(*id).map(|c| c.owner.clone());
            match found {
                None => return Ok(Some(privileges)),
                Some(o) if o.as_deref() != Some(owner.as_str()) => {
                    let msg = format!(
                        "cursor id {id} was not created by the authenticated user :: caused by :: User not authorized to access cursor"
                    );
                    return Err(CommandError::new(UNAUTHORIZED, msg));
                }
                Some(_) if !check.passes(&privileges, true) => {
                    let msg = format!("not authorized for getMore with cursor id {id}");
                    return Err(CommandError::new(UNAUTHORIZED, msg));
                }
                Some(_) => return Ok(Some(privileges)),
            }
        }
        Check::KillCursors { ids, .. } => {
            let g = lock(shared);
            let own = ids.iter().all(|id| {
                g.cursors.get(*id).is_none_or(|c| c.owner.as_deref() == Some(owner.as_str()))
            });
            drop(g);
            if !check.passes(&privileges, own) {
                return Err(unauthorized(req));
            }
        }
        _ if !check.passes(&privileges, true) => return Err(unauthorized(req)),
        _ => {}
    }
    // `rawData` also asks for `performRawDataOperations` on the collection (measured on a
    // `refnode` 9.0.4 with authentication on 02-11-2026: `find` alone, or the action alone, is
    // refused with 13).
    let raw = matches!(cmd.get("rawData"), Some(Value::Boolean(true)));
    if raw
        && let Some((_, Value::String(coll))) = cmd.iter().next()
        && !authz::allows(
            &privileges,
            &Target::collection(&req.db, coll),
            ActionType::PerformRawDataOperations,
        )
    {
        return Err(unauthorized(req));
    }
    Ok(Some(privileges))
}

/// The command with a collection given by its UUID named instead, as RefDb resolves it before
/// authorizing; the command itself when its first field is not a UUID of a collection of `db`.
fn by_name<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    cmd: &Document,
) -> Result<Document, CommandError> {
    let Some((field, Value::Binary { subtype: 4, bytes })) = cmd.iter().next() else {
        return Ok(cmd.clone());
    };
    let found = backend
        .collections()?
        .into_iter()
        .find(|(ns, c)| ns.db == db && c.uuid.as_slice() == bytes.as_slice());
    let Some((ns, _)) = found else { return Ok(cmd.clone()) };
    let mut out = Document::new();
    for (k, v) in cmd.iter() {
        if k == field {
            out.push(k, ns.coll.as_str());
        } else {
            out.push(k, v.clone());
        }
    }
    Ok(out)
}

/// `Unauthorized` for `req`: the command as it came in the body of the message, without the
/// fields that came as document sequences, with long strings cut and the password hidden.
pub fn unauthorized(req: &Request) -> CommandError {
    let hide = WITH_PASSWORD.contains(&req.name());
    let mut body = Document::new();
    for (k, v) in req.command.iter() {
        if req.sequences.iter().any(|s| s == k) {
            continue;
        }
        if hide && k == PASSWORD_FIELD {
            body.push(k, REDACTED);
        } else {
            body.push(k, quoted(v));
        }
    }
    let msg = format!(
        "not authorized on {} to execute command {}",
        req.db,
        display(&Value::Document(body))
    );
    CommandError::new(UNAUTHORIZED, msg)
}

/// `v` with each string of [`QUOTE_CUT_FROM`] bytes or more cut to [`QUOTE_KEEP`] and `...`.
/// RefDb cuts at the byte even inside a character, which leaves the reply with invalid UTF-8;
/// this cuts at the last character boundary before it (DV-044).
fn quoted(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() >= QUOTE_CUT_FROM => {
            let mut end = QUOTE_KEEP;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}...", &s[..end]))
        }
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, x) in d.iter() {
                out.push(k, quoted(x));
            }
            Value::Document(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(quoted).collect()),
        other => other.clone(),
    }
}

/// The privileges `listDatabases` filters its answer with, or `None` to list every database:
/// without authorization, with `authorizedDatabases: false` (which the check already asked
/// `listDatabases` for), or with `listDatabases` on the cluster and no `authorizedDatabases: true`.
pub fn database_filter<B: Backend + ?Sized>(backend: &B, cmd: &Document) -> Option<Vec<Privilege>> {
    let privileges = backend.privileges()?;
    match cmd.get("authorizedDatabases") {
        Some(Value::Boolean(false)) => None,
        Some(Value::Boolean(true)) => Some(privileges),
        _ if authz::allows(&privileges, &Target::Cluster, ActionType::ListDatabases) => None,
        _ => Some(privileges),
    }
}

/// The privileges `listCollections` on `db` filters its answer with, or `None` to list every
/// collection: only with `nameOnly` and `authorizedCollections` and without `listCollections` on
/// the database.
pub fn collection_filter<B: Backend + ?Sized>(
    backend: &B,
    db: &str,
    cmd: &Document,
) -> Option<Vec<Privilege>> {
    let privileges = backend.privileges()?;
    let on = |f: &str| matches!(cmd.get(f), Some(Value::Boolean(true)));
    let all = authz::allows(&privileges, &Target::database(db), ActionType::ListCollections);
    (on("nameOnly") && on("authorizedCollections") && !all).then_some(privileges)
}

/// Whether a filter of [`collection_filter`] shows collection `coll` of `db`.
pub fn collection_shown(filter: Option<&[Privilege]>, db: &str, coll: &str) -> bool {
    let t = Target::collection(db, coll);
    filter.is_none_or(|p| p.iter().any(|x| x.resource.covers(&t)))
}

/// Whether a filter of [`database_filter`] shows database `db`.
pub fn database_shown(filter: Option<&[Privilege]>, db: &str) -> bool {
    filter.is_none_or(|p| authz::touches(p, db))
}
