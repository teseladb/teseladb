//! User commands (013 FR-009, FR-013): `createUser`, `updateUser`, `dropUser`,
//! `dropAllUsersFromDatabase`, `grantRolesToUser`, `revokeRolesFromUser`, `usersInfo` and
//! `invalidateUserCache`, over the users of Group 0. A change reads the user, computes the new
//! document and writes it expecting the one it read, and starts again if another change came
//! first. Fields, checks and messages are the reference server 9.0.4's (measured on the reference
//! router with `--keyFile`).

use teseladb_auth::sasl::SaslMechanism;
use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, User, UserName};
use teseladb_auth::{restrictions, roles};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::security::{Expect, Principal, SecurityError, SecurityOp};

use super::args::{self, missing};
use super::authz_writes::{USER_FIELDS, key_error};
use super::{Backend, Ctx, new_uuid};
use crate::error::{
    BAD_VALUE, CommandError, IDL_UNKNOWN_FIELD, PASSWORD_NOT_TERMINATED, ROLE_NOT_FOUND,
    UNAUTHORIZED, USER_ALREADY_EXISTS, USER_NOT_FOUND, WRITE_CONFLICT,
};
use crate::types::{truthy, wrong_type, wrong_types};

/// `createUser`.
pub const CREATE_USER: &str = "createUser";
/// `updateUser`.
pub const UPDATE_USER: &str = "updateUser";
/// `dropUser`.
pub const DROP_USER: &str = "dropUser";
/// `dropAllUsersFromDatabase`.
pub const DROP_ALL_USERS: &str = "dropAllUsersFromDatabase";
/// `grantRolesToUser`.
pub const GRANT_ROLES_TO_USER: &str = "grantRolesToUser";
/// `revokeRolesFromUser`.
pub const REVOKE_ROLES_FROM_USER: &str = "revokeRolesFromUser";
/// `usersInfo`.
pub const USERS_INFO: &str = "usersInfo";
/// `invalidateUserCache`.
pub const INVALIDATE_USER_CACHE: &str = "invalidateUserCache";

/// The commands of this module.
pub const NAMES: [&str; 8] = [
    CREATE_USER,
    UPDATE_USER,
    DROP_USER,
    DROP_ALL_USERS,
    GRANT_ROLES_TO_USER,
    REVOKE_ROLES_FROM_USER,
    USERS_INFO,
    INVALIDATE_USER_CACHE,
];

/// The types the `show*` flags of `usersInfo` take, in the reference server's order.
const SHOW_TYPES: [&str; 5] = ["int", "double", "long", "decimal", "bool"];
/// Times a change reads and writes again after a concurrent change before giving up.
pub(super) const MAX_ATTEMPTS: usize = 16;

/// Runs the user command of `ctx`; `None` if it is not one of this module's.
pub fn run<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Option<Result<Document, CommandError>> {
    Some(match ctx.name() {
        CREATE_USER => create_user(ctx),
        UPDATE_USER => update_user(ctx),
        DROP_USER => drop_user(ctx),
        DROP_ALL_USERS => drop_all_users(ctx),
        GRANT_ROLES_TO_USER | REVOKE_ROLES_FROM_USER => change_roles(ctx),
        USERS_INFO => users_info(ctx),
        INVALIDATE_USER_CACHE => invalidate_user_cache(ctx),
        _ => return None,
    })
}

pub(super) fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

pub(super) fn bad_value(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg)
}

/// The user named by the command's first field, in the command's database.
fn target<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Result<UserName, CommandError> {
    let name = ctx.name().to_owned();
    match ctx.first() {
        Value::String(u) => Ok(UserName::new(u, ctx.db)),
        other => Err(wrong_type(&format!("{name}.{name}"), other, "string")),
    }
}

/// A role of a `roles` array: a name in `db`, or `{role, db}`.
pub(super) fn role_name(v: &Value, db: &str) -> Result<RoleName, CommandError> {
    match v {
        Value::String(r) => Ok(RoleName::new(r, db)),
        Value::Document(d) => {
            let field = |f: &str| match d.get(f) {
                Some(Value::String(s)) => Ok(s.clone()),
                _ => Err(bad_value(format!("RoleName must contain a field named: {f}"))),
            };
            Ok(RoleName::new(field("role")?, field("db")?))
        }
        _ => Err(bad_value("Role name must be either a document or string")),
    }
}

/// The `roles` array of command `name`, without repeats.
pub(super) fn roles_arg(
    cmd: &Document,
    name: &str,
    db: &str,
) -> Result<Option<Vec<RoleName>>, CommandError> {
    let Some(items) = args::array(cmd, name, "roles")? else { return Ok(None) };
    let mut out: Vec<RoleName> = Vec::new();
    for v in items {
        let r = role_name(v, db)?;
        if !out.contains(&r) {
            out.push(r);
        }
    }
    Ok(Some(out))
}

/// Fails with `RoleNotFound` for the first role of `roles` that does not exist.
fn check_roles<B: Backend + ?Sized>(
    backend: &mut B,
    roles: &[RoleName],
) -> Result<(), CommandError> {
    for r in roles {
        if !roles::is_builtin(r) && backend.principal(Principal::Role, &r.db, &r.role)?.is_none() {
            let msg = format!("Could not find role: {}@{}", r.role, r.db);
            return Err(CommandError::new(ROLE_NOT_FOUND, msg));
        }
    }
    Ok(())
}

/// The `mechanisms` array, without repeats.
fn mechanisms_arg(cmd: &Document, name: &str) -> Result<Option<Vec<Mechanism>>, CommandError> {
    let Some(items) = args::array(cmd, name, "mechanisms")? else { return Ok(None) };
    if items.is_empty() {
        return Err(bad_value("mechanisms field must not be empty"));
    }
    let mut out = Vec::new();
    for (i, v) in items.iter().enumerate() {
        let Value::String(m) = v else {
            return Err(wrong_type(&format!("{name}.mechanisms.{i}"), v, "string"));
        };
        let mech = Mechanism::from_name(m)
            .ok_or_else(|| bad_value(format!("Unknown auth mechanism '{m}'")))?;
        if !out.contains(&mech) {
            out.push(mech);
        }
    }
    Ok(Some(out))
}

/// `authenticationRestrictions`: documents with `clientSource` and `serverAddress`, arrays of
/// addresses in CIDR notation.
pub(super) fn restrictions_arg(
    cmd: &Document,
    name: &str,
) -> Result<Option<Vec<Value>>, CommandError> {
    const FIELD: &str = "authenticationRestrictions";
    let Some(items) = args::array(cmd, name, FIELD)? else { return Ok(None) };
    for (i, item) in items.iter().enumerate() {
        let Value::Document(d) = item else {
            return Err(wrong_type(&format!("{name}.{FIELD}.{i}"), item, "object"));
        };
        for (k, v) in d.iter() {
            let path = format!("{name}.{FIELD}.{k}");
            if k != "clientSource" && k != "serverAddress" {
                let msg = format!("BSON field '{path}' is an unknown field.");
                return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
            }
            let Value::Array(addrs) = v else { return Err(wrong_type(&path, v, "array")) };
            for a in addrs {
                if !matches!(a, Value::String(s) if restrictions::cidr(s).is_some()) {
                    return Err(bad_value("Invalid IP address in CIDR string"));
                }
            }
        }
    }
    Ok(Some(items.to_vec()))
}

/// The password of command `name`: a string, never empty.
fn password(cmd: &Document, name: &str) -> Result<Option<String>, CommandError> {
    match args::string(cmd, name, "pwd")? {
        // The reference server's SASLprep of an empty password fails with this ICU message
        // (measured).
        Some("") => Err(CommandError::new(
            PASSWORD_NOT_TERMINATED,
            "Error preflighting UTF-8 conversion: U_STRING_NOT_TERMINATED_WARNING",
        )),
        other => Ok(other.map(str::to_owned)),
    }
}

/// The credentials of `user` for `mechs` from `pwd`; with `digested` (`digestPassword: false`)
/// the client sent the SCRAM-SHA-1 digest itself.
fn credentials<B: Backend + ?Sized>(
    backend: &mut B,
    user: &mut User,
    mechs: &[Mechanism],
    pwd: &str,
    digested: bool,
) -> Result<(), CommandError> {
    if digested && mechs.contains(&Mechanism::ScramSha256) {
        return Err(bad_value("Use of SCRAM-SHA-256 requires undigested passwords"));
    }
    user.scram_sha1 = None;
    user.scram_sha256 = None;
    for &m in mechs {
        let prepared = if digested {
            pwd.as_bytes().to_vec()
        } else {
            m.prepare_password(&user.name.user, pwd, true)
                .map_err(|e| bad_value(format!("Error preparing the password: {e}")))?
        };
        let mut salt = vec![0u8; m.salt_len()];
        backend.secure_random(&mut salt)?;
        let creds = Credentials::derive(m, &prepared, &salt, m.default_iterations());
        match m {
            Mechanism::ScramSha1 => user.scram_sha1 = Some(creds),
            Mechanism::ScramSha256 => user.scram_sha256 = Some(creds),
        }
    }
    Ok(())
}

fn encode(user: &User) -> Result<Vec<u8>, CommandError> {
    Ok(user.to_document().to_bytes()?)
}

pub(super) fn decode(bytes: &[u8]) -> Result<User, CommandError> {
    let doc = Document::from(RawDoc::from_bytes(bytes)?);
    User::from_document(&doc).map_err(super::authz_collections::stored_error)
}

fn user_op(name: &UserName, expect: Expect, doc: Option<Vec<u8>>) -> SecurityOp {
    SecurityOp::named(Principal::User, &name.db, &name.user, expect, doc)
}

pub(super) fn conflict() -> CommandError {
    CommandError::new(WRITE_CONFLICT, "the users changed while the command ran; try again")
}

/// Reads user `name`, changes it with `change` and writes it back, again while another change
/// comes first. `missing` is the error when the user does not exist.
fn modify<B: Backend + ?Sized>(
    backend: &mut B,
    name: &UserName,
    missing_user: impl Fn() -> CommandError,
    mut change: impl FnMut(&mut B, &mut User) -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    for _ in 0..MAX_ATTEMPTS {
        let Some(old) = backend.principal(Principal::User, &name.db, &name.user)? else {
            return Err(missing_user());
        };
        let mut user = decode(&old)?;
        change(backend, &mut user)?;
        let new = super::authz_writes::merged(&old, &user.to_document(), &USER_FIELDS)?;
        let op = SecurityOp::put(Principal::User, Expect::Exactly(old), new).map_err(key_error)?;
        match backend.security_write(vec![op])? {
            Ok(()) => return Ok(()),
            Err(SecurityError::NotFound(_)) => return Err(missing_user()),
            Err(SecurityError::Exists(_) | SecurityError::Conflict) => {}
        }
    }
    Err(conflict())
}

/// `createUser`.
fn create_user<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    let cmd = ctx.cmd;
    let roles =
        roles_arg(cmd, CREATE_USER, ctx.db)?.ok_or_else(|| missing(CREATE_USER, "roles"))?;
    let pwd = password(cmd, CREATE_USER)?;
    let custom = args::doc(cmd, CREATE_USER, "customData")?.cloned();
    let restrictions = restrictions_arg(cmd, CREATE_USER)?;
    let mechs = mechanisms_arg(cmd, CREATE_USER)?;
    let digest = args::safe_bool(cmd, CREATE_USER, "digestPassword", &["bool"])?.unwrap_or(true);
    if name.user.is_empty() {
        return Err(bad_value("User document needs 'user' field to be non-empty"));
    }
    let external = name.db == EXTERNAL_DB;
    match (&pwd, external) {
        (None, false) => {
            return Err(bad_value(
                "Must provide a 'pwd' field for all user documents, except those with '$external' as the user's source db",
            ));
        }
        (Some(_), true) => {
            return Err(bad_value(
                "Cannot set the password for users defined on the '$external' database",
            ));
        }
        _ => {}
    }
    let mut user = User::new(name.clone());
    user.user_id = Some(new_uuid(ctx.backend));
    user.roles = roles;
    user.custom_data = custom;
    user.restrictions = restrictions;
    // A user belongs to the tenant of the session that creates it (007 FR-025).
    user.tenant = ctx.backend.tenant();
    if let Some(p) = &pwd {
        let mechs = mechs.unwrap_or_else(|| vec![Mechanism::ScramSha1, Mechanism::ScramSha256]);
        credentials(ctx.backend, &mut user, &mechs, p, !digest)?;
    }
    check_roles(ctx.backend, &user.roles)?;
    let op = user_op(&name, Expect::Absent, Some(encode(&user)?));
    match ctx.backend.security_write(vec![op])? {
        Ok(()) => Ok(ok()),
        Err(_) => {
            let msg = format!("User \"{}@{}\" already exists", name.user, name.db);
            Err(CommandError::new(USER_ALREADY_EXISTS, msg))
        }
    }
}

/// `updateUser`.
fn update_user<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    let cmd = ctx.cmd;
    let roles = roles_arg(cmd, UPDATE_USER, ctx.db)?;
    let pwd = password(cmd, UPDATE_USER)?;
    let custom = args::doc(cmd, UPDATE_USER, "customData")?.cloned();
    let restrictions = restrictions_arg(cmd, UPDATE_USER)?;
    let mechs = mechanisms_arg(cmd, UPDATE_USER)?;
    let digest = args::safe_bool(cmd, UPDATE_USER, "digestPassword", &["bool"])?.unwrap_or(true);
    if roles.is_none()
        && pwd.is_none()
        && custom.is_none()
        && restrictions.is_none()
        && mechs.is_none()
    {
        return Err(bad_value("Must specify at least one field to update in updateUser"));
    }
    if pwd.is_some() && name.db == EXTERNAL_DB {
        return Err(bad_value(
            "Cannot set the password for users defined on the '$external' database",
        ));
    }
    if let Some(r) = &roles {
        check_roles(ctx.backend, r)?;
    }
    let not_found =
        || CommandError::new(USER_NOT_FOUND, format!("User {}@{} not found", name.user, name.db));
    modify(ctx.backend, &name, not_found, |backend, user| {
        let held: Vec<Mechanism> = [Mechanism::ScramSha1, Mechanism::ScramSha256]
            .into_iter()
            .filter(|m| user.credentials(*m).is_some())
            .collect();
        match (&pwd, &mechs) {
            (Some(p), m) => {
                // A user that is not of `$external` always holds some mechanism.
                let mechs = m.clone().unwrap_or_else(|| held.clone());
                credentials(backend, user, &mechs, p, !digest)?;
            }
            (None, Some(m)) => {
                if m.iter().any(|x| !held.contains(x)) {
                    return Err(bad_value(
                        "mechanisms field must be a subset of previously set mechanisms",
                    ));
                }
                if !m.contains(&Mechanism::ScramSha1) {
                    user.scram_sha1 = None;
                }
                if !m.contains(&Mechanism::ScramSha256) {
                    user.scram_sha256 = None;
                }
            }
            (None, None) => {}
        }
        if let Some(r) = &roles {
            user.roles = r.clone();
        }
        if let Some(c) = &custom {
            user.custom_data = Some(c.clone());
        }
        if let Some(r) = &restrictions {
            user.restrictions = Some(r.clone());
        }
        Ok(())
    })?;
    Ok(ok())
}

/// `dropUser`.
fn drop_user<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    for _ in 0..MAX_ATTEMPTS {
        let Some(old) = ctx.backend.principal(Principal::User, &name.db, &name.user)? else {
            return Err(CommandError::new(
                USER_NOT_FOUND,
                format!("User '{}@{}' not found", name.user, name.db),
            ));
        };
        let op = SecurityOp::remove(Principal::User, old).map_err(key_error)?;
        if ctx.backend.security_write(vec![op])?.is_ok() {
            return Ok(ok());
        }
    }
    Err(conflict())
}

/// `dropAllUsersFromDatabase`: `n` users removed.
fn drop_all_users<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    for _ in 0..MAX_ATTEMPTS {
        let mut ops = Vec::new();
        for doc in ctx.backend.principals(Principal::User, Some(ctx.db))? {
            ops.push(SecurityOp::remove(Principal::User, doc).map_err(key_error)?);
        }
        let n = i32::try_from(ops.len()).unwrap_or(i32::MAX);
        if ops.is_empty() || ctx.backend.security_write(ops)?.is_ok() {
            return Ok(Document::new().with("n", n).with("ok", 1.0));
        }
    }
    Err(conflict())
}

/// `grantRolesToUser` and `revokeRolesFromUser`.
fn change_roles<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = target(ctx)?;
    let cmd_name = ctx.name().to_owned();
    let roles =
        roles_arg(ctx.cmd, &cmd_name, ctx.db)?.ok_or_else(|| missing(&cmd_name, "roles"))?;
    if roles.is_empty() {
        return Err(bad_value(format!("{cmd_name} command requires a non-empty \"roles\" array")));
    }
    check_roles(ctx.backend, &roles)?;
    let grant = cmd_name == GRANT_ROLES_TO_USER;
    let not_found = || {
        let msg = format!("Could not find user \"{}\" for db \"{}\"", name.user, name.db);
        CommandError::new(USER_NOT_FOUND, msg)
    };
    modify(ctx.backend, &name, not_found, |_, user| {
        if grant {
            for r in &roles {
                if !user.roles.contains(r) {
                    user.roles.push(r.clone());
                }
            }
        } else {
            user.roles.retain(|r| !roles.contains(r));
        }
        Ok(())
    })?;
    Ok(ok())
}

/// A `show*` flag of `usersInfo`.
pub(super) fn show(
    cmd: &Document,
    name: &str,
    field: &str,
    default: bool,
) -> Result<bool, CommandError> {
    match cmd.get(field) {
        None => Ok(default),
        Some(v) => truthy(v).ok_or_else(|| wrong_types(&format!("{name}.{field}"), v, &SHOW_TYPES)),
    }
}

/// What `usersInfo` asks for.
enum Wanted {
    /// Every user of the command's database.
    Database,
    /// Every user of every database.
    AllDatabases,
    /// These users.
    Users(Vec<UserName>),
}

/// A user name of `usersInfo`: a name in `db`, or `{user, db}`.
fn wanted_user(v: &Value, db: &str) -> Result<UserName, CommandError> {
    match v {
        Value::String(u) => Ok(UserName::new(u, db)),
        Value::Document(d) => {
            for (k, _) in d.iter() {
                if k != "user" && k != "db" {
                    return Err(bad_value(format!(
                        "UserName contains an unknown field named: '{k}"
                    )));
                }
            }
            let field = |f: &str| match d.get(f) {
                Some(Value::String(s)) => Ok(s.clone()),
                _ => Err(bad_value(format!("UserName must contain a field named: {f}"))),
            };
            Ok(UserName::new(field("user")?, field("db")?))
        }
        _ => Err(bad_value("UserName must be either a string or an object")),
    }
}

fn wanted(v: &Value, db: &str) -> Result<Wanted, CommandError> {
    match v {
        Value::Int32(1) | Value::Int64(1) => Ok(Wanted::Database),
        Value::Double(x) if *x == 1.0 => Ok(Wanted::Database),
        Value::Document(d) if d.get("forAllDBs") == Some(&Value::Boolean(true)) && d.len() == 1 => {
            Ok(Wanted::AllDatabases)
        }
        Value::Array(items) => {
            Ok(Wanted::Users(items.iter().map(|i| wanted_user(i, db)).collect::<Result<_, _>>()?))
        }
        other => Ok(Wanted::Users(vec![wanted_user(other, db)?])),
    }
}

/// The mechanisms `authenticatedMechanism` of `usersInfo` takes, in the order the reference server
/// 9.0.4 lists them (measured).
const AUTHENTICATED_MECHANISMS: [SaslMechanism; 7] = [
    SaslMechanism::X509,
    SaslMechanism::Plain,
    SaslMechanism::Gssapi,
    SaslMechanism::ScramSha1,
    SaslMechanism::ScramSha256,
    SaslMechanism::Aws,
    SaslMechanism::Oidc,
];

/// Checks `authenticatedMechanism` (013 FR-033). With LDAP authorization the reference server 9.0.4
/// still answers from the stored documents for every mechanism (measured), so it only chooses what
/// the command needs (`internal` on the cluster).
fn authenticated_mechanism(cmd: &Document) -> Result<(), CommandError> {
    match cmd.get("authenticatedMechanism") {
        None => Ok(()),
        Some(Value::String(m)) if AUTHENTICATED_MECHANISMS.iter().any(|x| x.name() == m) => Ok(()),
        Some(Value::String(m)) => {
            let names: Vec<&str> = AUTHENTICATED_MECHANISMS.iter().map(|x| x.name()).collect();
            let msg = format!(
                "Unknown authentication mechanism '{m}'. Supported mechanisms: {}",
                names.join(", ")
            );
            Err(CommandError::new(BAD_VALUE, msg))
        }
        Some(v) => Err(wrong_type(&format!("{USERS_INFO}.authenticatedMechanism"), v, "string")),
    }
}

/// `usersInfo`.
fn users_info<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let cmd = ctx.cmd;
    let credentials = show(cmd, USERS_INFO, "showCredentials", false)?;
    let custom = show(cmd, USERS_INFO, "showCustomData", true)?;
    let privileges = show(cmd, USERS_INFO, "showPrivileges", false)?;
    let restrictions = show(cmd, USERS_INFO, "showAuthenticationRestrictions", false)?;
    let filter = args::doc(cmd, USERS_INFO, "filter")?;
    authenticated_mechanism(cmd)?;
    let wanted = wanted(ctx.first(), ctx.db)?;
    principals_barrier(ctx)?;
    let docs = match wanted {
        Wanted::Database => ctx.backend.principals(Principal::User, Some(ctx.db))?,
        Wanted::AllDatabases => ctx.backend.principals(Principal::User, None)?,
        Wanted::Users(names) => {
            let mut found: Vec<Vec<u8>> = Vec::new();
            for n in names {
                if let Some(d) = ctx.backend.principal(Principal::User, &n.db, &n.user)?
                    && !found.contains(&d)
                {
                    found.push(d);
                }
            }
            found
        }
    };
    let shown = super::users_info::Shown { credentials, custom, restrictions };
    let mut out = Vec::new();
    for d in super::users_info::sorted(docs)? {
        let d = if privileges || restrictions {
            let mut user = User::new(UserName::new("", ""));
            user.roles = teseladb_auth::users::parse_roles(&d)
                .map_err(super::authz_collections::stored_error)?;
            let inherited = super::roles::user_inherited(ctx.backend, &user)?;
            let roles: Vec<Value> = inherited.roles.iter().map(RoleName::to_value).collect();
            let privs: Vec<Value> = inherited.privileges.iter().map(|p| p.to_value()).collect();
            let fields = [
                ("inheritedRoles", Value::Array(roles)),
                ("inheritedPrivileges", Value::Array(privs)),
                ("inheritedAuthenticationRestrictions", Value::Array(inherited.restrictions)),
            ];
            super::users_info::with_inherited(&d, shown, fields)?
        } else {
            super::users_info::plain(&d, shown)?
        };
        if super::catalog::passes(filter, &d)? {
            out.push(Value::Document(d));
        }
    }
    Ok(Document::new().with("users", out).with("ok", 1.0))
}

/// `invalidateUserCache`: TeselaDB reads the users from its copy of Group 0 on each command; the
/// only cache to empty is that of the users' LDAP groups (013 FR-026).
fn invalidate_user_cache<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    if ctx.db != roles::ADMIN_DB {
        let msg = "invalidateUserCache may only be run against the admin database.";
        return Err(CommandError::new(UNAUTHORIZED, msg));
    }
    ctx.backend.invalidate_user_cache();
    Ok(ok())
}

/// The barrier the reads of users and roles of a command take first (013 T058): this node's copy of Group 0 then
/// holds every change any node committed before the command.
pub(crate) fn principals_barrier<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<(), CommandError> {
    ctx.backend.catalog_barrier()
}
