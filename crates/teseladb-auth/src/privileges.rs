//! Privileges (FR-010): a resource and the actions allowed on it, read from and written to the
//! documents of `createRole`, `rolesInfo` and `admin.system.roles`, with the reference server
//! 9.0.4's checks and messages (measured), and the privileges of the built-in roles of a database.

use teseladb_bson::{Document, Value};

use crate::actions::ActionType;
use crate::builtin_roles::{self, ROLE_DB};
use crate::roles::{self, ADMIN_DB};
use crate::users::RoleName;

/// A resource of the built-in table, with [`ROLE_DB`] for the role's database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Res {
    /// `{cluster: true}`.
    Cluster,
    /// `{anyResource: true}`.
    Any,
    /// `{db, collection}`.
    Ns(&'static str, &'static str),
    /// `{system_buckets}`.
    SystemBuckets(&'static str),
}

/// A resource.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resource {
    /// `{cluster: true}`.
    Cluster,
    /// `{anyResource: true}`.
    Any,
    /// `{db, collection}`; an empty string is any database or any collection.
    Ns {
        /// Database.
        db: String,
        /// Collection.
        collection: String,
    },
    /// `{db, system_buckets}`, or `{system_buckets}` alone.
    SystemBuckets {
        /// Database, when given.
        db: Option<String>,
        /// Bucket collection.
        bucket: String,
    },
}

/// A resource and its actions, in the reference server's order and without repeats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Privilege {
    /// What it applies to.
    pub resource: Resource,
    /// What it allows.
    pub actions: Vec<ActionType>,
}

/// Why a privilege document was refused; the wire layer gives each its code.
#[derive(Clone, Debug, PartialEq)]
pub enum PrivilegeError {
    /// A required field is missing (`IDLFailedToParse`); the path after the command.
    Missing(&'static str),
    /// A field of the wrong type (`TypeMismatch`): path, value, expected type.
    WrongType(&'static str, Value, &'static str),
    /// `BadValue` with this message.
    Bad(String),
    /// A role of a database other than `admin` granted something outside it
    /// (`InvalidRoleModification`).
    OutsideDatabase(String),
}

impl ActionType {
    /// The action called `name`.
    pub fn from_name(name: &str) -> Option<ActionType> {
        ActionType::ALL.into_iter().find(|a| a.name() == name)
    }
}

impl Resource {
    fn from_static(r: Res, db: &str) -> Resource {
        let real = |d: &str| if d == ROLE_DB { db.to_owned() } else { d.to_owned() };
        match r {
            Res::Cluster => Resource::Cluster,
            Res::Any => Resource::Any,
            Res::Ns(d, c) => Resource::Ns { db: real(d), collection: c.to_owned() },
            Res::SystemBuckets(b) => Resource::SystemBuckets { db: None, bucket: b.to_owned() },
        }
    }

    /// The resource document.
    pub fn to_document(&self) -> Document {
        match self {
            Resource::Cluster => Document::new().with("cluster", true),
            Resource::Any => Document::new().with("anyResource", true),
            Resource::Ns { db, collection } => {
                Document::new().with("db", db.as_str()).with("collection", collection.as_str())
            }
            Resource::SystemBuckets { db, bucket } => {
                let mut d = Document::new();
                if let Some(db) = db {
                    d.push("db", db.as_str());
                }
                d.push("system_buckets", bucket.as_str());
                d
            }
        }
    }

    fn from_document(d: &Document) -> Result<Resource, PrivilegeError> {
        let text = |f: &'static str, path: &'static str| match d.get(f) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(v) => Err(PrivilegeError::WrongType(path, v.clone(), "string")),
        };
        if d.get("cluster") == Some(&Value::Boolean(true)) {
            return Ok(Resource::Cluster);
        }
        if d.get("anyResource") == Some(&Value::Boolean(true)) {
            return Ok(Resource::Any);
        }
        let db = text("db", "privileges.resource.db")?;
        let collection = text("collection", "privileges.resource.collection")?;
        let bucket = text("system_buckets", "privileges.resource.system_buckets")?;
        match (db, collection, bucket) {
            (Some(db), Some(collection), None) => Ok(Resource::Ns { db, collection }),
            (db, None, Some(bucket)) => Ok(Resource::SystemBuckets { db, bucket }),
            _ => Err(PrivilegeError::Bad(
                "resource pattern must contain 'collection' or 'systemBuckets' specifier".into(),
            )),
        }
    }

    /// Whether a role of database `db` may hold it: a role of `admin` anything, another only
    /// its own database.
    fn allowed_for(&self, db: &str) -> bool {
        // A `system_buckets` resource only in `admin`, also on the role's own database
        // (measured on a reference replica set member 9.0.4 on 02-11-2026: 49).
        db == ADMIN_DB || matches!(self, Resource::Ns { db: d, .. } if d == db)
    }
}

impl Privilege {
    /// `{resource, actions}`.
    pub fn to_value(&self) -> Value {
        let actions: Vec<Value> = self.actions.iter().map(|a| Value::from(a.name())).collect();
        Value::Document(
            Document::new().with("resource", self.resource.to_document()).with("actions", actions),
        )
    }

    /// Reads a privilege of a role of database `db`.
    pub fn from_value(v: &Value, db: &str) -> Result<Privilege, PrivilegeError> {
        let Value::Document(d) = v else {
            return Err(PrivilegeError::WrongType("privileges", v.clone(), "object"));
        };
        let resource = match d.get("resource") {
            None => return Err(PrivilegeError::Missing("privileges.resource")),
            Some(Value::Document(r)) => Resource::from_document(r)?,
            Some(other) => {
                return Err(PrivilegeError::WrongType(
                    "privileges.resource",
                    other.clone(),
                    "object",
                ));
            }
        };
        let names = match d.get("actions") {
            None => return Err(PrivilegeError::Missing("privileges.actions")),
            Some(Value::Array(a)) => a,
            Some(other) => {
                return Err(PrivilegeError::WrongType(
                    "privileges.actions",
                    other.clone(),
                    "array",
                ));
            }
        };
        if names.is_empty() {
            return Err(PrivilegeError::Bad(
                "'actions' field of privilege resource must not be empty".into(),
            ));
        }
        let mut actions = Vec::new();
        for n in names {
            let Value::String(n) = n else {
                return Err(PrivilegeError::WrongType("privileges.actions", n.clone(), "string"));
            };
            let a = ActionType::from_name(n).ok_or_else(|| {
                PrivilegeError::Bad(format!("Unknown action type in privilege set: '{n}'"))
            })?;
            actions.push(a);
        }
        actions.sort();
        actions.dedup();
        if !resource.allowed_for(db) {
            return Err(PrivilegeError::OutsideDatabase(format!(
                "Roles on the '{db}' database cannot be granted privileges that target other databases or the cluster"
            )));
        }
        Ok(Privilege { resource, actions })
    }
}

/// The actions on the cluster a user of a tenant other than the default one keeps (013 FR-024,
/// D-116): those that only reach its own databases or its own command. Every other action on the
/// cluster (`shutdown`, `getLog`, `inprog`, `serverStatus`...) concerns what all tenants share.
pub const TENANT_CLUSTER_ACTIONS: [ActionType; 3] =
    [ActionType::BypassDefaultMaxTimeMS, ActionType::ListDatabases, ActionType::UseUUID];

/// Privileges `privs` as a user of a tenant other than the default one has them (013 FR-024):
/// on the cluster only [`TENANT_CLUSTER_ACTIONS`], and nothing on `anyResource`; its databases
/// are its tenant's, so those privileges stay.
pub fn of_tenant(privs: Vec<Privilege>) -> Vec<Privilege> {
    privs
        .into_iter()
        .filter_map(|p| match p.resource {
            Resource::Any => None,
            Resource::Cluster => {
                let actions: Vec<ActionType> =
                    p.actions.into_iter().filter(|a| TENANT_CLUSTER_ACTIONS.contains(a)).collect();
                (!actions.is_empty()).then_some(Privilege { resource: Resource::Cluster, actions })
            }
            _ => Some(p),
        })
        .collect()
}

/// Adds `more` to `set`, merging the actions of privileges on the same resource, as the reference
/// server merges them when it lists inherited privileges.
pub fn merge(set: &mut Vec<Privilege>, more: &[Privilege]) {
    for p in more {
        match set.iter_mut().find(|q| q.resource == p.resource) {
            Some(q) => {
                q.actions.extend(p.actions.iter().copied());
                q.actions.sort();
                q.actions.dedup();
            }
            None => set.push(p.clone()),
        }
    }
}

/// Removes `less` from `set`: the actions of each privilege on the same resource, and the
/// privilege when none is left.
pub fn subtract(set: &mut Vec<Privilege>, less: &[Privilege]) {
    for p in less {
        if let Some(q) = set.iter_mut().find(|q| q.resource == p.resource) {
            q.actions.retain(|a| !p.actions.contains(a));
        }
    }
    set.retain(|q| !q.actions.is_empty());
}

/// The privileges of built-in role `role`, or `None` if it is not one.
pub fn builtin(role: &RoleName) -> Option<Vec<Privilege>> {
    if !roles::is_builtin(role) {
        return None;
    }
    let table = builtin_roles::privileges(&role.role)?;
    Some(
        table
            .iter()
            .map(|(r, actions)| Privilege {
                resource: Resource::from_static(*r, &role.db),
                actions: actions.to_vec(),
            })
            .collect(),
    )
}
