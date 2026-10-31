//! The `param` document of each audit action type (013 FR-016), with the fields and order that
//! RefDb Enterprise 9.0.4 writes (measured), and the arguments of an authorization check
//! without passwords or document values.

use teseladb_bson::{Document, Value};

use crate::users::{RoleName, UserName};

/// What RefDb writes in place of a password.
pub const PASSWORD_MASK: &str = "xxx";
/// What TeselaDB writes in place of a value of the client's data (DV of 013 FR-016).
pub const VALUE_MASK: &str = "###";
/// The field of a password.
const PASSWORD_FIELD: &str = "pwd";
/// The field of the database of a command.
const DB_FIELD: &str = "$db";

/// The stage of an index build in `createIndex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexBuildState {
    /// The build began.
    Started,
    /// The build ended and the index is ready.
    Succeeded,
}

impl IndexBuildState {
    /// Its name in `indexBuildState`.
    pub fn name(self) -> &'static str {
        match self {
            IndexBuildState::Started => "IndexBuildStarted",
            IndexBuildState::Succeeded => "IndexBuildSucceeded",
        }
    }
}

fn roles_value(roles: &[RoleName]) -> Value {
    Value::Array(roles.iter().map(RoleName::to_value).collect())
}

fn users_value(users: &[UserName]) -> Value {
    Value::Array(
        users
            .iter()
            .map(|u| {
                Value::Document(
                    Document::new().with("user", u.user.as_str()).with("db", u.db.as_str()),
                )
            })
            .collect(),
    )
}

/// `authenticate`: the user, its database and the mechanism.
pub fn authenticate(user: &UserName, mechanism: &str) -> Document {
    Document::new()
        .with("user", user.user.as_str())
        .with("db", user.db.as_str())
        .with("mechanism", mechanism)
}

/// `authCheck`: the command, its namespace and, when given, its arguments without passwords or
/// document values. A command refused because the connection has not authenticated goes
/// without arguments and with the database as namespace (measured).
pub fn auth_check(command: &str, ns: &str, args: Option<&Document>) -> Document {
    let doc = Document::new().with("command", command).with("ns", ns);
    match args {
        Some(a) => doc.with("args", redact_args(a)),
        None => doc,
    }
}

/// The namespace RefDb names in `authCheck`: `db.<first value>` when the command's first value
/// is a non-empty string, else the database.
pub fn auth_check_ns(db: &str, cmd: &Document) -> String {
    match cmd.iter().next() {
        Some((_, Value::String(s))) if !s.is_empty() => format!("{db}.{s}"),
        _ => db.to_owned(),
    }
}

/// A command's arguments as the audit log keeps them: its name, `$db` and every top-level
/// scalar as they came, a password as [`PASSWORD_MASK`], and every value inside a document or
/// an array (documents, filters, updates, pipelines) as [`VALUE_MASK`], with its field names.
pub fn redact_args(cmd: &Document) -> Document {
    let mut out = Document::new();
    for (k, v) in cmd.iter() {
        let kept = match v {
            _ if k == PASSWORD_FIELD => Value::String(PASSWORD_MASK.to_owned()),
            _ if k == DB_FIELD => v.clone(),
            Value::Document(_) | Value::Array(_) => mask(v),
            other => other.clone(),
        };
        out.push(k, kept);
    }
    out
}

fn mask(v: &Value) -> Value {
    match v {
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, inner) in d.iter() {
                out.push(k, mask(inner));
            }
            Value::Document(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(mask).collect()),
        _ => Value::String(VALUE_MASK.to_owned()),
    }
}

/// `createCollection`, `dropCollection`, `createDatabase` and `dropDatabase`: the namespace.
pub fn namespace(ns: &str) -> Document {
    Document::new().with("ns", ns)
}

/// `createIndex`: the namespace, the index, its specification and the stage of the build.
pub fn create_index(ns: &str, name: &str, spec: &Document, state: IndexBuildState) -> Document {
    Document::new()
        .with("ns", ns)
        .with("indexName", name)
        .with("indexSpec", spec.clone())
        .with("indexBuildState", state.name())
}

/// `dropIndex`: the namespace and the index.
pub fn drop_index(ns: &str, name: &str) -> Document {
    Document::new().with("ns", ns).with("indexName", name)
}

/// `renameCollection`: the old and the new namespace.
pub fn rename_collection(old: &str, new: &str) -> Document {
    Document::new().with("old", old).with("new", new)
}

/// `createUser`: the user and its roles (never its password).
pub fn create_user(user: &UserName, roles: &[RoleName]) -> Document {
    user_fields(user).with("roles", roles_value(roles))
}

/// `dropUser`: the user.
pub fn drop_user(user: &UserName) -> Document {
    user_fields(user)
}

/// `updateUser`: the user, whether its password changed, and its custom data and roles when the
/// command set them.
pub fn update_user(
    user: &UserName,
    password_changed: bool,
    custom_data: Option<&Document>,
    roles: Option<&[RoleName]>,
) -> Document {
    let mut doc = user_fields(user).with("passwordChanged", password_changed);
    if let Some(c) = custom_data {
        doc.push("customData", c.clone());
    }
    if let Some(r) = roles {
        doc.push("roles", roles_value(r));
    }
    doc
}

/// `grantRolesToUser` and `revokeRolesFromUser`: the user and the roles.
pub fn user_roles(user: &UserName, roles: &[RoleName]) -> Document {
    user_fields(user).with("roles", roles_value(roles))
}

fn user_fields(user: &UserName) -> Document {
    Document::new().with("user", user.user.as_str()).with("db", user.db.as_str())
}

fn role_fields(role: &RoleName) -> Document {
    Document::new().with("role", role.role.as_str()).with("db", role.db.as_str())
}

/// `createRole`: the role, the roles it inherits and its privileges, as the command gave them.
pub fn create_role(role: &RoleName, roles: &[RoleName], privileges: &[Value]) -> Document {
    role_fields(role)
        .with("roles", roles_value(roles))
        .with("privileges", Value::Array(privileges.to_vec()))
}

/// `updateRole`: the role and what the command replaced.
pub fn update_role(
    role: &RoleName,
    roles: Option<&[RoleName]>,
    privileges: Option<&[Value]>,
) -> Document {
    let mut doc = role_fields(role);
    if let Some(r) = roles {
        doc.push("roles", roles_value(r));
    }
    if let Some(p) = privileges {
        doc.push("privileges", Value::Array(p.to_vec()));
    }
    doc
}

/// `dropRole`: the role.
pub fn drop_role(role: &RoleName) -> Document {
    role_fields(role)
}

/// `grantRolesToRole` and `revokeRolesFromRole`: the role and the roles.
pub fn role_roles(role: &RoleName, roles: &[RoleName]) -> Document {
    role_fields(role).with("roles", roles_value(roles))
}

/// `grantPrivilegesToRole` and `revokePrivilegesFromRole`: the role and the privileges.
pub fn role_privileges(role: &RoleName, privileges: &[Value]) -> Document {
    role_fields(role).with("privileges", Value::Array(privileges.to_vec()))
}

/// `dropAllUsersFromDatabase` and `dropAllRolesFromDatabase`: the database.
pub fn database(db: &str) -> Document {
    Document::new().with("db", db)
}

/// Why a `logout` happened, as RefDb words it.
pub const DISCONNECTED: &str = "Client has disconnected";

/// `logout`: why, the users before and after, and when they logged in.
pub fn logout(reason: &str, initial: &[UserName], updated: &[UserName], login_ms: i64) -> Document {
    Document::new()
        .with("reason", reason)
        .with("initialUsers", users_value(initial))
        .with("updatedUsers", users_value(updated))
        .with("loginTime", Value::DateTime(login_ms))
}

/// `clientMetadata`: the node's end of the connection and the metadata of the first `hello`.
pub fn client_metadata(local: &Document, metadata: &Document) -> Document {
    Document::new().with("localEndpoint", local.clone()).with("clientMetadata", metadata.clone())
}

/// `startup`: the options the node started with.
pub fn startup(options: &Document) -> Document {
    Document::new().with("startupOptions", options.clone())
}

/// `shutdown`: nothing.
pub fn shutdown() -> Document {
    Document::new()
}
