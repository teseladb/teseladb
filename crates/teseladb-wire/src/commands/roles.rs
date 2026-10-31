//! Role commands (013 FR-009, FR-010): `createRole`, `updateRole`, `dropRole`,
//! `dropAllRolesFromDatabase`, `grantPrivilegesToRole`, `revokePrivilegesFromRole`,
//! `grantRolesToRole`, `revokeRolesFromRole` and `rolesInfo`, and the role graph that gives a
//! user or a role what it inherits. Built-in roles cannot be changed; a role cannot inherit from
//! itself; dropping a role takes it from every user and role in the same Group 0 batch. Fields,
//! checks, field order and messages are RefDb 9.0.4's (measured on a `refrouter` with
//! `--keyFile`). RefDb lists inherited roles in an order that changes between processes;
//! TeselaDB lists them breadth first from the roles granted.

use std::collections::BTreeMap;

use teseladb_auth::privileges::{self, Privilege, PrivilegeError};
use teseladb_auth::roles::{self, Role};
use teseladb_auth::users::{RoleName, User};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::security::{Expect, Principal, SecurityError, SecurityOp};

use super::args::{self, missing};
use super::authz_writes::{ROLE_FIELDS, key_error};
use super::users::{
    MAX_ATTEMPTS, bad_value, conflict, ok, restrictions_arg, role_name, roles_arg, show,
};
use super::{Backend, Ctx};
use crate::error::{
    CommandError, ILLEGAL_OPERATION, INVALID_ROLE_MODIFICATION, ROLE_ALREADY_EXISTS, ROLE_NOT_FOUND,
};
use crate::types::wrong_type;

/// `createRole`.
pub const CREATE_ROLE: &str = "createRole";
/// `updateRole`.
pub const UPDATE_ROLE: &str = "updateRole";
/// `dropRole`.
pub const DROP_ROLE: &str = "dropRole";
/// `dropAllRolesFromDatabase`.
pub const DROP_ALL_ROLES: &str = "dropAllRolesFromDatabase";
/// `grantPrivilegesToRole`.
pub const GRANT_PRIVILEGES: &str = "grantPrivilegesToRole";
/// `revokePrivilegesFromRole`.
pub const REVOKE_PRIVILEGES: &str = "revokePrivilegesFromRole";
/// `grantRolesToRole`.
pub const GRANT_ROLES_TO_ROLE: &str = "grantRolesToRole";
/// `revokeRolesFromRole`.
pub const REVOKE_ROLES_FROM_ROLE: &str = "revokeRolesFromRole";
/// `rolesInfo`.
pub const ROLES_INFO: &str = "rolesInfo";

/// The commands of this module.
pub const NAMES: [&str; 9] = [
    CREATE_ROLE,
    UPDATE_ROLE,
    DROP_ROLE,
    DROP_ALL_ROLES,
    GRANT_PRIVILEGES,
    REVOKE_PRIVILEGES,
    GRANT_ROLES_TO_ROLE,
    REVOKE_ROLES_FROM_ROLE,
    ROLES_INFO,
];

/// `showPrivileges: "asUserFragment"`.
const AS_USER_FRAGMENT: &str = "asUserFragment";

/// Runs the role command of `ctx`; `None` if it is not one of this module's.
pub fn run<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Option<Result<Document, CommandError>> {
    Some(match ctx.name() {
        CREATE_ROLE => create_role(ctx),
        UPDATE_ROLE
        | GRANT_PRIVILEGES
        | REVOKE_PRIVILEGES
        | GRANT_ROLES_TO_ROLE
        | REVOKE_ROLES_FROM_ROLE => change_role(ctx),
        DROP_ROLE => drop_role(ctx),
        DROP_ALL_ROLES => drop_all_roles(ctx),
        ROLES_INFO => roles_info(ctx),
        _ => return None,
    })
}

fn decode(bytes: &[u8]) -> Result<Role, CommandError> {
    let doc = Document::from(RawDoc::from_bytes(bytes)?);
    Role::from_document(&doc).map_err(super::authz_collections::stored_error)
}

/// A custom role as `rolesInfo` reads it (measured on RefDb 9.0.4): its stored document, which
/// it shows as it is, the role with the privileges that parse, and those that do not, which it
/// still lists as inherited. `name` prefixes the error of a role asked by name.
struct Listed {
    role: Role,
    raw: Document,
    unparsed: Vec<Value>,
}

fn listed(bytes: &[u8], by_name: bool) -> Result<Listed, CommandError> {
    let raw = Document::from(RawDoc::from_bytes(bytes)?);
    let fail = |e: CommandError| match by_name {
        true => {
            let text = |f: &str| match raw.get(f) {
                Some(Value::String(t)) => t.clone(),
                _ => String::new(),
            };
            let shown = format!("{}@{}", text("role"), text("db"));
            CommandError::new(e.code, format!("Failed fetching role '{shown}': {}", e.errmsg))
        }
        false => e,
    };
    match raw.get("roles") {
        Some(Value::Array(a)) => {
            for r in a {
                RoleName::parse_stored(r)
                    .map_err(super::authz_collections::stored_error)
                    .map_err(fail)?;
            }
        }
        Some(_) => {
            return Err(fail(CommandError::new(
                crate::error::BAD_VALUE,
                "Invalid roles field, expected array",
            )));
        }
        None => {}
    }
    let privileges = match raw.get("privileges") {
        Some(Value::Array(a)) => a.clone(),
        Some(_) => {
            let msg = "invalid parameter: expected an object (privileges)";
            return Err(fail(CommandError::new(crate::error::EXPECTED_OBJECT, msg)));
        }
        None => Vec::new(),
    };
    let with = |privs: Vec<Value>| {
        let mut d = raw.clone();
        if d.get("privileges").is_some() {
            d.insert("privileges", Value::Array(privs));
        }
        d
    };
    let (good, unparsed): (Vec<Value>, Vec<Value>) =
        privileges.into_iter().partition(|p| Role::from_document(&with(vec![p.clone()])).is_ok());
    let role = Role::from_document(&with(good))
        .map_err(super::authz_collections::stored_error)
        .map_err(fail)?;
    Ok(Listed { role, raw, unparsed })
}

fn role_op(name: &RoleName, expect: Expect, doc: Option<Vec<u8>>) -> SecurityOp {
    SecurityOp::named(Principal::Role, &name.db, &name.role, expect, doc)
}

/// What the roles of a user or a role give: every role reached, the privileges merged and the
/// authentication restrictions of each role that has them.
#[derive(Default)]
pub struct Inherited {
    /// Roles reached, breadth first.
    pub roles: Vec<RoleName>,
    /// Their privileges, merged by resource.
    pub privileges: Vec<Privilege>,
    /// The `authenticationRestrictions` of each role that has them.
    pub restrictions: Vec<Value>,
}

/// The roles read so far by one command, so that each is read once.
pub struct Graph<'a, B: Backend + ?Sized> {
    backend: &'a mut B,
    seen: BTreeMap<RoleName, Option<Role>>,
}

impl<'a, B: Backend + ?Sized> Graph<'a, B> {
    /// A graph over the roles of `backend`.
    pub fn new(backend: &'a mut B) -> Graph<'a, B> {
        Graph { backend, seen: BTreeMap::new() }
    }

    /// Role `name`: built-in or custom; `None` if it does not exist.
    pub fn role(&mut self, name: &RoleName) -> Result<Option<Role>, CommandError> {
        if let Some(r) = self.seen.get(name) {
            return Ok(r.clone());
        }
        let r = match Role::builtin(name) {
            Some(b) => Some(b),
            None => match self.backend.principal(Principal::Role, &name.db, &name.role)? {
                Some(bytes) => Some(decode(&bytes)?),
                None => None,
            },
        };
        self.seen.insert(name.clone(), r.clone());
        Ok(r)
    }

    /// What `direct` gives, with `own` privileges and restrictions first (a role's own).
    pub fn inherit(
        &mut self,
        direct: &[RoleName],
        own: Option<&Role>,
    ) -> Result<Inherited, CommandError> {
        let mut out = Inherited::default();
        if let Some(r) = own {
            privileges::merge(&mut out.privileges, &r.privileges);
            if let Some(x) = &r.restrictions {
                out.restrictions.push(Value::Array(x.clone()));
            }
        }
        let mut queue: Vec<RoleName> = direct.to_vec();
        let mut i = 0;
        while i < queue.len() {
            let name = queue[i].clone();
            i += 1;
            if out.roles.contains(&name) {
                continue;
            }
            let Some(role) = self.role(&name)? else { continue };
            out.roles.push(name);
            privileges::merge(&mut out.privileges, &role.privileges);
            if let Some(x) = &role.restrictions {
                out.restrictions.push(Value::Array(x.clone()));
            }
            queue.extend(role.roles.iter().cloned());
        }
        Ok(out)
    }

    /// Fails with `RoleNotFound`, as `{prefix}Could not find role: …`, for the first of `names`
    /// that does not exist.
    fn check_exist(&mut self, names: &[RoleName], prefix: &str) -> Result<(), CommandError> {
        for r in names {
            if self.role(r)?.is_none() {
                let msg = format!("{prefix}Could not find role: {}@{}", r.role, r.db);
                return Err(CommandError::new(ROLE_NOT_FOUND, msg));
            }
        }
        Ok(())
    }
}

fn values(names: &[RoleName]) -> Value {
    Value::Array(names.iter().map(RoleName::to_value).collect())
}

fn privilege_values(p: &[Privilege]) -> Value {
    Value::Array(p.iter().map(Privilege::to_value).collect())
}

/// The `privileges` array of command `name` for a role of `db`.
fn privileges_arg(
    cmd: &Document,
    name: &str,
    db: &str,
) -> Result<Option<Vec<Privilege>>, CommandError> {
    let Some(items) = args::array(cmd, name, "privileges")? else { return Ok(None) };
    let mut out = Vec::new();
    for v in items {
        let p = Privilege::from_value(v, db).map_err(|e| match e {
            PrivilegeError::Missing(path) => missing(name, path),
            PrivilegeError::WrongType(path, v, t) => wrong_type(&format!("{name}.{path}"), &v, t),
            PrivilegeError::Bad(m) => bad_value(m),
            PrivilegeError::OutsideDatabase(m) => CommandError::new(INVALID_ROLE_MODIFICATION, m),
        })?;
        privileges::merge(&mut out, &[p]);
    }
    Ok(Some(out))
}

/// The role named by the command's first field, in the command's database.
fn target<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Result<RoleName, CommandError> {
    let name = ctx.name().to_owned();
    match ctx.first() {
        Value::String(r) => Ok(RoleName::new(r, ctx.db)),
        other => Err(wrong_type(&format!("{name}.{name}"), other, "string")),
    }
}

fn cycle(name: &RoleName) -> CommandError {
    let msg = format!(
        "Granting roles to {}@{} would introduce a cycle in the role graph",
        name.role, name.db
    );
    CommandError::new(INVALID_ROLE_MODIFICATION, msg)
}

/// `createRole`.
fn create_role<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    let cmd = ctx.cmd;
    let privileges = privileges_arg(cmd, CREATE_ROLE, ctx.db)?
        .ok_or_else(|| missing(CREATE_ROLE, "privileges"))?;
    let granted =
        roles_arg(cmd, CREATE_ROLE, ctx.db)?.ok_or_else(|| missing(CREATE_ROLE, "roles"))?;
    let restrictions = restrictions_arg(cmd, CREATE_ROLE)?;
    if name.role.is_empty() {
        return Err(bad_value("Role name must be non-empty"));
    }
    if roles::is_builtin_name(&name.role) {
        return Err(bad_value("Cannot create roles with the same name as a built-in role"));
    }
    let prefix = format!("Cannot grant roles to '{}@{}': ", name.role, name.db);
    Graph::new(ctx.backend).check_exist(&granted, &prefix)?;
    let role = Role { name: name.clone(), privileges, roles: granted, restrictions };
    let op = role_op(&name, Expect::Absent, Some(role.to_document().to_bytes()?));
    match ctx.backend.security_write(vec![op])? {
        Ok(()) => Ok(ok()),
        Err(_) => Err(CommandError::new(
            ROLE_ALREADY_EXISTS,
            format!("Role \"{}@{}\" already exists", name.role, name.db),
        )),
    }
}

/// `updateRole` and the commands that grant or revoke privileges or roles.
fn change_role<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    let cmd_name = ctx.name().to_owned();
    let cmd = ctx.cmd;
    let privileges = privileges_arg(cmd, &cmd_name, ctx.db)?;
    let granted = roles_arg(cmd, &cmd_name, ctx.db)?;
    let restrictions = restrictions_arg(cmd, &cmd_name)?;
    let not_found =
        || CommandError::new(ROLE_NOT_FOUND, format!("Role {}@{} not found", name.role, name.db));
    match cmd_name.as_str() {
        UPDATE_ROLE => {
            if privileges.is_none() && granted.is_none() && restrictions.is_none() {
                return Err(bad_value("Must specify at least one field to update in updateRole"));
            }
            if roles::is_builtin(&name) {
                return Err(not_found());
            }
        }
        GRANT_PRIVILEGES | REVOKE_PRIVILEGES => {
            let p = privileges.as_ref().ok_or_else(|| missing(&cmd_name, "privileges"))?;
            if p.is_empty() {
                let msg = format!("{cmd_name} command requires a non-empty \"privileges\" array");
                return Err(bad_value(msg));
            }
        }
        _ => {
            let r = granted.as_ref().ok_or_else(|| missing(&cmd_name, "roles"))?;
            if r.is_empty() {
                return Err(bad_value(format!(
                    "{cmd_name} command requires a non-empty \"roles\" array"
                )));
            }
        }
    }
    if cmd_name != UPDATE_ROLE && roles::is_builtin(&name) {
        return Err(bad_value(format!(
            "{}@{} is a built-in role and cannot be modified",
            name.role, name.db
        )));
    }
    if let (Some(g), true) = (&granted, cmd_name != REVOKE_ROLES_FROM_ROLE) {
        let mut graph = Graph::new(ctx.backend);
        graph.check_exist(g, "")?;
    }
    for _ in 0..MAX_ATTEMPTS {
        let Some(old) = ctx.backend.principal(Principal::Role, &name.db, &name.role)? else {
            return Err(if cmd_name == UPDATE_ROLE {
                CommandError::new(
                    ROLE_NOT_FOUND,
                    format!("Could not find role: {}@{}", name.role, name.db),
                )
            } else {
                not_found()
            });
        };
        let mut role = decode(&old)?;
        match cmd_name.as_str() {
            UPDATE_ROLE => {
                if let Some(p) = &privileges {
                    role.privileges = p.clone();
                }
                if let Some(r) = &granted {
                    role.roles = r.clone();
                }
                if let Some(r) = &restrictions {
                    role.restrictions = Some(r.clone());
                }
            }
            GRANT_PRIVILEGES => {
                privileges::merge(&mut role.privileges, privileges.as_deref().unwrap_or(&[]))
            }
            REVOKE_PRIVILEGES => {
                privileges::subtract(&mut role.privileges, privileges.as_deref().unwrap_or(&[]));
            }
            GRANT_ROLES_TO_ROLE => {
                for r in granted.iter().flatten() {
                    if !role.roles.contains(r) {
                        role.roles.push(r.clone());
                    }
                }
            }
            _ => {
                let gone = granted.as_deref().unwrap_or(&[]);
                role.roles.retain(|r| !gone.contains(r));
            }
        }
        // A role that reaches itself through its roles would be a cycle.
        let mut graph = Graph::new(ctx.backend);
        if role.roles.contains(&name) || graph.inherit(&role.roles, None)?.roles.contains(&name) {
            return Err(cycle(&name));
        }
        let new = super::authz_writes::merged(&old, &role.to_document(), &ROLE_FIELDS)?;
        let op = SecurityOp::put(Principal::Role, Expect::Exactly(old), new).map_err(key_error)?;
        match ctx.backend.security_write(vec![op])? {
            Ok(()) => return Ok(ok()),
            Err(SecurityError::NotFound(_)) => return Err(not_found()),
            Err(SecurityError::Exists(_) | SecurityError::Conflict) => {}
        }
    }
    Err(conflict())
}

/// The ops that take role `name` from every user and custom role that holds it.
fn strip<B: Backend + ?Sized>(
    backend: &mut B,
    names: &[RoleName],
) -> Result<Vec<SecurityOp>, CommandError> {
    let mut ops = Vec::new();
    for kind in [Principal::User, Principal::Role] {
        for bytes in backend.principals(kind, None)? {
            // The roles being dropped go whole.
            let d = Document::from(RawDoc::from_bytes(&bytes)?);
            let named = match (d.get("role"), d.get("db")) {
                (Some(Value::String(r)), Some(Value::String(db))) => Some(RoleName::new(r, db)),
                _ => None,
            };
            if kind == Principal::Role && named.is_some_and(|n| names.contains(&n)) {
                continue;
            }
            if let Some(new) = super::authz_writes::without_roles(&bytes, names)? {
                ops.push(SecurityOp::put(kind, Expect::Exactly(bytes), new).map_err(key_error)?);
            }
        }
    }
    Ok(ops)
}

/// `dropRole`.
fn drop_role<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    if roles::is_builtin(&name) {
        return Err(bad_value(format!(
            "{}@{} is a built-in role and cannot be modified",
            name.role, name.db
        )));
    }
    for _ in 0..MAX_ATTEMPTS {
        let Some(old) = ctx.backend.principal(Principal::Role, &name.db, &name.role)? else {
            let msg = format!("Could not find role: {}@{}", name.role, name.db);
            return Err(CommandError::new(ROLE_NOT_FOUND, msg));
        };
        let mut ops = vec![SecurityOp::remove(Principal::Role, old).map_err(key_error)?];
        ops.extend(strip(ctx.backend, std::slice::from_ref(&name))?);
        if ctx.backend.security_write(ops)?.is_ok() {
            return Ok(ok());
        }
    }
    Err(conflict())
}

/// `dropAllRolesFromDatabase`: `n` roles removed.
fn drop_all_roles<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    for _ in 0..MAX_ATTEMPTS {
        let mut ops = Vec::new();
        let mut names = Vec::new();
        for bytes in ctx.backend.principals(Principal::Role, Some(ctx.db))? {
            let d = Document::from(RawDoc::from_bytes(&bytes)?);
            if let (Some(Value::String(r)), Some(Value::String(db))) = (d.get("role"), d.get("db"))
            {
                names.push(RoleName::new(r, db));
            }
            ops.push(SecurityOp::remove(Principal::Role, bytes).map_err(key_error)?);
        }
        let n = i32::try_from(ops.len()).unwrap_or(i32::MAX);
        if ops.is_empty() {
            return Ok(Document::new().with("n", n).with("ok", 1.0));
        }
        ops.extend(strip(ctx.backend, &names)?);
        if ctx.backend.security_write(ops)?.is_ok() {
            return Ok(Document::new().with("n", n).with("ok", 1.0));
        }
    }
    Err(conflict())
}

/// What `rolesInfo` asks for.
enum Wanted {
    /// Every role of the command's database.
    Database,
    /// These roles.
    Roles(Vec<RoleName>),
}

fn wanted_role(v: &Value, db: &str) -> Result<RoleName, CommandError> {
    match v {
        Value::String(_) | Value::Document(_) => role_name(v, db),
        _ => Err(bad_value("RoleName must be either a string or an object")),
    }
}

/// What `rolesInfo` shows.
struct Show {
    privileges: bool,
    fragment: bool,
    restrictions: bool,
    builtin: bool,
}

/// A role as `rolesInfo` shows it: `listing` for a listing of the database, which places
/// `isBuiltin` before the inherited fields (measured).
fn info<B: Backend + ?Sized>(
    graph: &mut Graph<'_, B>,
    (role, stored): (&Role, Option<&Listed>),
    show: &Show,
    listing: bool,
) -> Result<Document, CommandError> {
    let inherited = graph.inherit(&role.roles, Some(role))?;
    let raw_field = |f: &str| stored.and_then(|l| l.raw.get(f).cloned());
    let builtin = roles::is_builtin(&role.name);
    let mut d = Document::new();
    if builtin && listing {
        d.push("role", role.name.role.as_str());
        d.push("db", role.name.db.as_str());
        d.push("isBuiltin", true);
        d.push("roles", values(&role.roles));
        d.push("inheritedRoles", values(&inherited.roles));
        if show.privileges {
            d.push("privileges", role.privileges_value());
            d.push("inheritedPrivileges", privilege_values(&inherited.privileges));
        }
        return Ok(d);
    }
    if builtin {
        d.push("db", role.name.db.as_str());
        d.push("role", role.name.role.as_str());
    } else {
        let id = format!("{}.{}", role.name.db, role.name.role);
        d.push("_id", raw_field("_id").unwrap_or(Value::from(id)));
        d.push("role", role.name.role.as_str());
        d.push("db", role.name.db.as_str());
    }
    // A built-in role by name lists `roles` before `privileges`, a custom one after (measured);
    // a custom one shows its stored fields as they are, and leaves out those it does not have.
    if builtin {
        d.push("roles", values(&role.roles));
    }
    if show.privileges {
        match stored {
            Some(_) => {
                if let Some(p) = raw_field("privileges") {
                    d.push("privileges", p);
                }
            }
            None => d.push("privileges", role.privileges_value()),
        }
    }
    if !builtin {
        match stored {
            Some(_) => {
                if let Some(r) = raw_field("roles") {
                    d.push("roles", r);
                }
            }
            None => d.push("roles", values(&role.roles)),
        }
    }
    let unparsed = stored.map(|l| l.unparsed.clone()).unwrap_or_default();
    if let (true, Some(r)) = (show.restrictions, &role.restrictions) {
        d.push("authenticationRestrictions", Value::Array(vec![Value::Array(r.clone())]));
    }
    if listing {
        d.push("isBuiltin", false);
    }
    d.push("inheritedRoles", values(&inherited.roles));
    if show.privileges {
        let mut privs = match privilege_values(&inherited.privileges) {
            Value::Array(a) => a,
            other => vec![other],
        };
        privs.extend(unparsed);
        d.push("inheritedPrivileges", privs);
    }
    if show.restrictions {
        d.push("inheritedAuthenticationRestrictions", Value::Array(inherited.restrictions));
    }
    if !listing {
        d.push("isBuiltin", builtin);
    }
    Ok(d)
}

/// `rolesInfo`.
fn roles_info<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let cmd = ctx.cmd;
    let fragment =
        matches!(cmd.get("showPrivileges"), Some(Value::String(s)) if s == AS_USER_FRAGMENT);
    let privileges = fragment || show(cmd, ROLES_INFO, "showPrivileges", false)?;
    let restrictions = show(cmd, ROLES_INFO, "showAuthenticationRestrictions", false)?;
    let builtin = show(cmd, ROLES_INFO, "showBuiltinRoles", false)?;
    let s = Show { privileges, fragment, restrictions, builtin };
    let wanted = match ctx.first() {
        Value::Int32(1) | Value::Int64(1) => Wanted::Database,
        Value::Double(x) if *x == 1.0 => Wanted::Database,
        Value::Array(items) => {
            Wanted::Roles(items.iter().map(|v| wanted_role(v, ctx.db)).collect::<Result<_, _>>()?)
        }
        other => Wanted::Roles(vec![wanted_role(other, ctx.db)?]),
    };
    let db = ctx.db.to_owned();
    let mut graph = Graph::new(ctx.backend);
    let (listing, mut found) = match wanted {
        Wanted::Database => {
            if s.fragment {
                let msg = "Cannot get user fragment for all roles in a database";
                return Err(CommandError::new(ILLEGAL_OPERATION, msg));
            }
            let mut all = Vec::new();
            if s.builtin {
                let mut names: Vec<&str> = roles::DATABASE_ROLES.to_vec();
                if db == roles::ADMIN_DB {
                    names.extend(roles::ADMIN_ROLES);
                }
                all.extend(names.into_iter().filter_map(|n| Role::builtin(&RoleName::new(n, &db))));
            }
            let mut all: Vec<(Role, Option<Listed>)> = all.into_iter().map(|r| (r, None)).collect();
            for bytes in graph.backend.principals(Principal::Role, Some(&db))? {
                let l = listed(&bytes, false)?;
                all.push((l.role.clone(), Some(l)));
            }
            (true, all)
        }
        Wanted::Roles(names) => {
            let mut all = Vec::new();
            for n in &names {
                if let Some(b) = Role::builtin(n) {
                    all.push((b, None));
                } else if let Some(bytes) =
                    graph.backend.principal(Principal::Role, &n.db, &n.role)?
                {
                    let l = listed(&bytes, true)?;
                    all.push((l.role.clone(), Some(l)));
                }
            }
            (false, all)
        }
    };
    if s.fragment {
        let direct: Vec<RoleName> = found.iter().map(|r| r.0.name.clone()).collect();
        let inherited = graph.inherit(&direct, None)?;
        let fragment = Document::new()
            .with("roles", values(&direct))
            .with("inheritedRoles", values(&inherited.roles))
            .with("inheritedPrivileges", privilege_values(&inherited.privileges));
        return Ok(Document::new().with("userFragment", fragment).with("ok", 1.0));
    }
    found.dedup_by(|a, b| a.0.name == b.0.name);
    let mut out = Vec::new();
    for (r, l) in &found {
        out.push(Value::Document(info(&mut graph, (r, l.as_ref()), &s, listing)?));
    }
    Ok(Document::new().with("roles", out).with("ok", 1.0))
}

/// The inherited fields `usersInfo` adds for a user with `showPrivileges` or
/// `showAuthenticationRestrictions`.
pub fn user_inherited<B: Backend + ?Sized>(
    backend: &mut B,
    user: &User,
) -> Result<Inherited, CommandError> {
    Graph::new(backend).inherit(&user.roles, None)
}
