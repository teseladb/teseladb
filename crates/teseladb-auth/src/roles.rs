//! Roles (FR-010): the built-in roles of the reference server 9.0 and the databases each one exists
//! on, and the custom roles with the document `admin.system.roles` keeps for them (FR-030)
//! (`rolesInfo` with `showBuiltinRoles` on a reference router 9.0.4, measured on `admin`, `config`,
//! `test` and `$external`).

use teseladb_bson::{Document, Value};

use crate::privileges::{Privilege, PrivilegeError};
use crate::users::{EXTERNAL_DB, RoleName};

/// The database of the cluster-wide roles.
pub const ADMIN_DB: &str = "admin";

/// Built-in roles of every database but `$external`.
pub const DATABASE_ROLES: [&str; 6] =
    ["read", "readWrite", "dbAdmin", "dbOwner", "userAdmin", "enableSharding"];

/// Built-in roles that exist only on `admin`.
pub const ADMIN_ROLES: [&str; 15] = [
    "readAnyDatabase",
    "readWriteAnyDatabase",
    "userAdminAnyDatabase",
    "dbAdminAnyDatabase",
    "clusterAdmin",
    "clusterManager",
    "clusterMonitor",
    "hostManager",
    "backup",
    "restore",
    "root",
    "__system",
    "__queryableBackup",
    "searchCoordinator",
    "directShardOperations",
];

/// Whether `role` is a built-in role.
pub fn is_builtin(role: &RoleName) -> bool {
    match role.db.as_str() {
        EXTERNAL_DB => false,
        ADMIN_DB => {
            DATABASE_ROLES.contains(&role.role.as_str())
                || ADMIN_ROLES.contains(&role.role.as_str())
        }
        _ => DATABASE_ROLES.contains(&role.role.as_str()),
    }
}

/// Whether `name` is the name of a built-in role on some database (a custom role cannot take it).
pub fn is_builtin_name(name: &str) -> bool {
    DATABASE_ROLES.contains(&name) || ADMIN_ROLES.contains(&name)
}

/// A role: its own privileges, the roles it inherits from and its `authenticationRestrictions`.
#[derive(Clone, Debug, PartialEq)]
pub struct Role {
    /// Its name.
    pub name: RoleName,
    /// The privileges granted to it directly.
    pub privileges: Vec<Privilege>,
    /// The roles granted to it, in the order they were granted.
    pub roles: Vec<RoleName>,
    /// `authenticationRestrictions`, as given.
    pub restrictions: Option<Vec<Value>>,
}

/// A stored role document that the reference server would not use.
pub type BadRoleDocument = crate::users::BadDocument;

impl Role {
    /// A built-in role, with its privileges; `None` if `name` is not one.
    pub fn builtin(name: &RoleName) -> Option<Role> {
        Some(Role {
            name: name.clone(),
            privileges: crate::privileges::builtin(name)?,
            roles: Vec::new(),
            restrictions: None,
        })
    }

    /// Its privileges as `[{resource, actions}]`.
    pub fn privileges_value(&self) -> Value {
        Value::Array(self.privileges.iter().map(Privilege::to_value).collect())
    }

    /// The roles it inherits from as `[{role, db}]`.
    pub fn roles_value(&self) -> Value {
        Value::Array(self.roles.iter().map(RoleName::to_value).collect())
    }

    /// The document of `admin.system.roles`.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new()
            .with("_id", format!("{}.{}", self.name.db, self.name.role))
            .with("role", self.name.role.as_str())
            .with("db", self.name.db.as_str())
            .with("privileges", self.privileges_value())
            .with("roles", self.roles_value());
        if let Some(r) = &self.restrictions {
            d.push("authenticationRestrictions", Value::Array(r.clone()));
        }
        d
    }

    /// Reads a stored role as the reference server 9.0.4 does when it resolves the roles of a user
    /// (measured): missing `privileges` or `roles` are empty, a role name may be the string
    /// `db.role`, unknown actions are left out; a field in the wrong form is the error the
    /// reference server answers.
    pub fn from_document(d: &Document) -> Result<Role, BadRoleDocument> {
        use crate::users::{BAD_VALUE, BadDocument, MISSING_FIELD, UNSUPPORTED_FORMAT};
        let text = |f: &str| match d.get(f) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(BadDocument::new(UNSUPPORTED_FORMAT, format!("no string '{f}' in role"))),
        };
        let name = RoleName::new(text("role")?, text("db")?);
        let shown = format!("{}@{}", name.role, name.db);
        let privileges = match d.get("privileges") {
            None => Vec::new(),
            Some(Value::Array(privs)) => privs
                .iter()
                .map(|p| {
                    if !matches!(p, Value::Document(_)) {
                        return Err(BadDocument::new(
                            UNSUPPORTED_FORMAT,
                            format!(
                                "Expected privilege document as object, got {}",
                                teseladb_mql::types::name_of(p.element_type())
                            ),
                        ));
                    }
                    Privilege::from_value(&known_actions(p), ADMIN_DB).map_err(|e| match e {
                        PrivilegeError::Missing(path) => BadDocument::new(
                            MISSING_FIELD,
                            format!(
                                "BSON field 'resolveRoles.{}' is missing but a required field",
                                path.trim_start_matches("privileges.")
                            ),
                        ),
                        PrivilegeError::WrongType(path, v, t) => BadDocument::new(
                            crate::users::EXPECTED_OBJECT,
                            format!(
                                "'{path}' should be {t}, found {}",
                                teseladb_mql::types::name_of(v.element_type())
                            ),
                        ),
                        PrivilegeError::Bad(m) | PrivilegeError::OutsideDatabase(m) => {
                            BadDocument::new(BAD_VALUE, m)
                        }
                    })
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(BadDocument::new(
                    UNSUPPORTED_FORMAT,
                    format!("Invalid 'privileges' field in role document '{shown}'"),
                ));
            }
        };
        let roles = match d.get("roles") {
            None => Vec::new(),
            Some(Value::Array(roles)) => {
                roles.iter().map(RoleName::parse_stored).collect::<Result<_, _>>()?
            }
            Some(other) => {
                return Err(BadDocument::new(
                    BAD_VALUE,
                    format!(
                        "Invalid 'roles' field in role document '{shown}', expected an array but found {}",
                        teseladb_mql::types::name_of(other.element_type())
                    ),
                ));
            }
        };
        let restrictions = match d.get("authenticationRestrictions") {
            None => None,
            Some(Value::Array(a)) => Some(a.clone()),
            Some(_) => {
                return Err(BadDocument::new(
                    UNSUPPORTED_FORMAT,
                    format!(
                        "Invalid 'authenticationRestrictions' field in role document '{shown}'"
                    ),
                ));
            }
        };
        Ok(Role { name, privileges, roles, restrictions })
    }
}

/// A stored privilege without the actions the reference server does not know, which it leaves out
/// when it resolves a role (measured: a privilege of only unknown actions is then empty).
fn known_actions(p: &Value) -> Value {
    let Value::Document(d) = p else { return p.clone() };
    let mut out = Document::new();
    for (k, v) in d.iter() {
        let v = match (k, v) {
            ("actions", Value::Array(a)) => Value::Array(
                a.iter()
                    .filter(|n| {
                        !matches!(n, Value::String(n) if crate::actions::ActionType::from_name(n).is_none())
                    })
                    .cloned()
                    .collect(),
            ),
            _ => v.clone(),
        };
        out.push(k, v);
    }
    Value::Document(out)
}
