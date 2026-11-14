//! `admin.system.users`, `admin.system.roles` and `admin.system.version` (013 FR-030): collections
//! that only read, whose documents come from Group 0, and `_mergeAuthzCollections`, with which
//! the reference restore tool with `--restoreDbUsersAndRoles` brings back the users and roles it
//! copied to temporary collections; as the reference server 9.0.4 answers them (measured on a
//! reference replica set member and the reference router, 31-10-2026).

use teseladb_auth::actions::ActionType;
use teseladb_auth::privileges::{Privilege, PrivilegeError};
use teseladb_auth::roles::ADMIN_DB;
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::Namespace;
use teseladb_meta::security::{Expect, Principal, SecurityError, SecurityOp, id_key, string_id};
use teseladb_range::read::ReadConcern;

use super::args::missing;
use super::names;
use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, CommandError, EXPECTED_OBJECT, FAILED_TO_PARSE, INVALID_NAMESPACE, NO_SUCH_KEY,
    ROLE_ALREADY_EXISTS, TYPE_MISMATCH, UNAUTHORIZED, USER_ALREADY_EXISTS,
};
use crate::types::{type_name, wrong_type};

/// The users' collection.
pub const SYSTEM_USERS: &str = "system.users";
/// The custom roles' collection.
pub const SYSTEM_ROLES: &str = "system.roles";
/// The versions' collection.
pub const SYSTEM_VERSION: &str = "system.version";
/// `_mergeAuthzCollections`.
pub const MERGE_AUTHZ_COLLECTIONS: &str = "_mergeAuthzCollections";
/// `authSchemaVersion` (measured: 5, and it cannot change).
pub const AUTH_SCHEMA_VERSION: i32 = 5;
/// The feature compatibility version `admin.system.version` holds.
const FEATURE_COMPATIBILITY_VERSION: &str = "9.0";
/// `_id` of its document.
const FCV_ID: &str = "featureCompatibilityVersion";
const TEMP_USERS: &str = "tempUsersCollection";
const TEMP_ROLES: &str = "tempRolesCollection";
const DROP: &str = "drop";
const DB: &str = "db";

/// One of the collections of `admin` generated from Group 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generated {
    /// `admin.system.users`.
    Users,
    /// `admin.system.roles`.
    Roles,
    /// `admin.system.version`.
    Version,
}

impl Generated {
    /// In the order `listCollections` of the reference router 9.0.4 lists them.
    pub const ALL: [Generated; 3] = [Generated::Users, Generated::Version, Generated::Roles];

    /// The generated collection `ns` is, if it is one.
    pub fn of(ns: &Namespace) -> Option<Generated> {
        if ns.db != ADMIN_DB {
            return None;
        }
        Generated::ALL.into_iter().find(|g| g.name() == ns.coll)
    }

    /// Its name in `admin`.
    pub fn name(self) -> &'static str {
        match self {
            Generated::Users => SYSTEM_USERS,
            Generated::Roles => SYSTEM_ROLES,
            Generated::Version => SYSTEM_VERSION,
        }
    }

    /// The UUID `listCollections` gives it: the same in every cluster, since the collection is
    /// a view of Group 0 that no node creates.
    pub fn uuid(self) -> [u8; 16] {
        let tag = match self {
            Generated::Users => 1,
            Generated::Roles => 2,
            Generated::Version => 3,
        };
        let mut u = [0x7e; 16];
        u[6] = 0x40;
        u[8] = 0x80;
        u[15] = tag;
        u
    }

    /// Its indexes, as `listIndexes` gives them.
    pub fn indexes(self) -> Vec<Document> {
        let id = Document::new()
            .with("v", 2)
            .with("key", Document::new().with("_id", 1))
            .with("name", "_id_");
        let unique = |field: &str| {
            Document::new()
                .with("v", 2)
                .with("key", Document::new().with(field, 1).with(DB, 1))
                .with("name", format!("{field}_1_db_1"))
                .with("unique", true)
        };
        match self {
            Generated::Users => vec![id, unique("user")],
            Generated::Roles => vec![id, unique("role")],
            Generated::Version => vec![id],
        }
    }

    /// Its documents: the users or roles of Group 0 in name order, or the feature compatibility
    /// version.
    pub fn documents<B: Backend + ?Sized>(
        self,
        backend: &mut B,
    ) -> Result<Vec<Document>, CommandError> {
        let kind = match self {
            Generated::Users => Principal::User,
            Generated::Roles => Principal::Role,
            Generated::Version => {
                let d = Document::new()
                    .with("_id", FCV_ID)
                    .with("version", FEATURE_COMPATIBILITY_VERSION);
                return Ok(vec![d]);
            }
        };
        let mut out = Vec::new();
        for bytes in backend.principals(kind, None)? {
            out.push(Document::from(RawDoc::from_bytes(&bytes)?));
        }
        Ok(out)
    }

    /// Whether it exists: `system.users` from the first user, `system.roles` from the first
    /// custom role, `system.version` always.
    pub fn exists<B: Backend + ?Sized>(self, backend: &mut B) -> Result<bool, CommandError> {
        Ok(match self {
            Generated::Users => !backend.principals(Principal::User, None)?.is_empty(),
            Generated::Roles => !backend.principals(Principal::Role, None)?.is_empty(),
            Generated::Version => true,
        })
    }
}

/// The documents of `ns` when it is a generated collection.
pub fn documents<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Option<Vec<Document>>, CommandError> {
    match Generated::of(ns) {
        Some(g) => g.documents(backend).map(Some),
        // `local.oplog.rs`, from the versions of the documents (018 FR-009).
        None if super::oplog::is_oplog(ns) => {
            super::oplog::documents(backend, &Document::new(), None).map(Some)
        }
        None if super::local_db::generated(ns) => super::local_db::documents(backend, ns).map(Some),
        // The collections of `config` that show the cluster (007 FR-017).
        None => super::config_db::documents(backend, ns),
    }
}

/// What `_mergeAuthzCollections` asks.
struct MergeArgs {
    users: Option<String>,
    roles: Option<String>,
    drop: bool,
    db: String,
}

impl MergeArgs {
    fn parse(cmd: &Document) -> Result<MergeArgs, CommandError> {
        let mut a = MergeArgs { users: None, roles: None, drop: false, db: String::new() };
        let mut db = None;
        for (k, v) in cmd.iter().skip(1) {
            let path = format!("{MERGE_AUTHZ_COLLECTIONS}.{k}");
            match (k, v) {
                (TEMP_USERS, Value::String(s)) => a.users = Some(s.clone()),
                (TEMP_ROLES, Value::String(s)) => a.roles = Some(s.clone()),
                (DROP, Value::Boolean(b)) => a.drop = *b,
                (DB, Value::String(s)) => db = Some(s.clone()),
                (TEMP_USERS | TEMP_ROLES | DB, v) => return Err(wrong_type(&path, v, "string")),
                (DROP, v) => return Err(wrong_type(&path, v, "bool")),
                _ => {}
            }
        }
        a.db = db.ok_or_else(|| missing(MERGE_AUTHZ_COLLECTIONS, DB))?;
        a.users = a.users.filter(|s| !s.is_empty());
        a.roles = a.roles.filter(|s| !s.is_empty());
        if a.users.is_none() && a.roles.is_none() {
            let msg =
                "Must provide at least one of \"tempUsersCollection\" and \"tempRolesCollection\"";
            return Err(CommandError::new(BAD_VALUE, msg));
        }
        Ok(a)
    }
}

/// `_mergeAuthzCollections`: brings the users and roles of the temporary collections into
/// Group 0. Without `drop` it only adds those that are missing; with `drop` the users and roles
/// of `db` (of every database when it is empty) become exactly those of the temporary
/// collections. A temporary collection that does not exist counts as empty; its documents of
/// another database are left out. Everything is checked before anything changes, and the change
/// is one batch of Group 0. A batch refused because a name or an `_id` is taken goes again while
/// what it would write changes; the same batch refused twice meets a user or role of another
/// tenant, which this tenant cannot see and names hold in the whole cluster (DV-023): the answer
/// is the one `createUser` or `createRole` gives.
pub fn merge<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    if ctx.db != ADMIN_DB {
        let msg = "_mergeAuthzCollections may only be run against the admin database.";
        return Err(CommandError::new(UNAUTHORIZED, msg));
    }
    let a = MergeArgs::parse(ctx.cmd)?;
    let users = match &a.users {
        Some(ns) => Some(read_temp(ctx.backend, ns, Principal::User, &a.db)?),
        None => None,
    };
    let roles = match &a.roles {
        Some(ns) => Some(read_temp(ctx.backend, ns, Principal::Role, &a.db)?),
        None => None,
    };
    let mut taken: Option<Vec<SecurityOp>> = None;
    for _ in 0..super::users::MAX_ATTEMPTS {
        let mut ops = Vec::new();
        if let Some(t) = &users {
            plan(ctx.backend, Principal::User, t, &a, &mut ops)?;
        }
        if let Some(t) = &roles {
            plan(ctx.backend, Principal::Role, t, &a, &mut ops)?;
        }
        if ops.is_empty() {
            return Ok(Document::new().with("ok", 1.0));
        }
        match ctx.backend.security_write(ops.clone())? {
            Ok(()) => return Ok(Document::new().with("ok", 1.0)),
            Err(SecurityError::Exists(_)) if taken.as_ref() == Some(&ops) => {
                return Err(exists(ctx.backend, &ops)?);
            }
            Err(SecurityError::Exists(_)) => taken = Some(ops),
            Err(SecurityError::NotFound(_) | SecurityError::Conflict) => taken = None,
        }
    }
    Err(super::users::conflict())
}

/// The error for the first user or role `ops` adds whose `_id`, or the `_id` `<db>.<name>` that
/// `createUser` and `createRole` give, another tenant holds (the first one it adds if none is):
/// `UserAlreadyExists` or `RoleAlreadyExists` with the words of those commands.
fn exists<B: Backend + ?Sized>(
    backend: &mut B,
    ops: &[SecurityOp],
) -> Result<CommandError, CommandError> {
    let mut added = Vec::new();
    for op in ops.iter().filter(|op| op.expect == Expect::Absent) {
        let Some(doc) = &op.doc else { continue };
        let d = Document::from(RawDoc::from_bytes(doc)?);
        added.push((op, text_field(&d, op.kind.name_field())?, text_field(&d, DB)?));
    }
    let mut culprit = None;
    for (op, name, db) in &added {
        let named = string_id(&format!("{db}.{name}"));
        if backend.principal_by_id(op.kind, &op.id)?.is_some()
            || backend.principal_by_id(op.kind, &named)?.is_some()
        {
            culprit = Some((op, name, db));
            break;
        }
    }
    let first = added.first().map(|(op, name, db)| (op, name, db));
    let Some((op, name, db)) = culprit.or(first) else { return Ok(super::users::conflict()) };
    Ok(match op.kind {
        Principal::Role => {
            CommandError::new(ROLE_ALREADY_EXISTS, format!("Role \"{name}@{db}\" already exists"))
        }
        Principal::User => {
            CommandError::new(USER_ALREADY_EXISTS, format!("User \"{name}@{db}\" already exists"))
        }
        Principal::ApiKey => no_collection(),
    })
}

/// A user or role of a temporary collection: database, name and the document Group 0 keeps.
struct Entry {
    db: String,
    name: String,
    doc: Vec<u8>,
}

/// The users or roles of temporary collection `full` that belong to `db` (all with an empty
/// `db`), checked and in the form Group 0 keeps them.
fn read_temp<B: Backend + ?Sized>(
    backend: &mut B,
    full: &str,
    kind: Principal,
    db: &str,
) -> Result<Vec<Entry>, CommandError> {
    let invalid = |shown: &str| {
        CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified: {shown}"))
    };
    let ns = names::parse_full(full).ok_or_else(|| invalid(full))?;
    names::check(&ns)?;
    let mut docs = Vec::new();
    if let Some(c) = backend.collection(&ns)? {
        let empty = Document::new();
        super::query::for_each_doc(
            backend,
            (&c, &empty),
            &empty,
            None,
            None,
            ReadConcern::Local,
            |_, bytes| {
                docs.push(Document::from(RawDoc::from_bytes(bytes)?));
                Ok(true)
            },
        )?;
    }
    let mut out = Vec::new();
    for d in docs {
        let entry = match kind {
            Principal::User => user_entry(&d)?,
            Principal::Role => role_entry(&d)?,
            Principal::ApiKey => return Err(no_collection()),
        };
        if db.is_empty() || entry.db == db {
            out.push(entry);
        }
    }
    Ok(out)
}

/// The error of a collection command on the API keys, which no collection shows (019 FR-016).
pub(crate) fn no_collection() -> CommandError {
    CommandError::new(crate::error::INTERNAL_ERROR, "the API keys have no collection")
}

/// `NoSuchKey` or `TypeMismatch` unless field `f` of `d` is a string, as the reference server
/// checks a stored user or role.
fn text_field(d: &Document, f: &str) -> Result<String, CommandError> {
    match d.get(f) {
        Some(Value::String(s)) => Ok(s.clone()),
        None => Err(CommandError::new(NO_SUCH_KEY, format!("Missing expected field \"{f}\""))),
        Some(v) => Err(CommandError::new(
            TYPE_MISMATCH,
            format!("\"{f}\" had the wrong type. Expected string, found {}", type_name(v)),
        )),
    }
}

/// `Location10065`: what the reference server answers for a field that is not the object or array
/// it reads.
fn expected_object(what: &str) -> CommandError {
    CommandError::new(EXPECTED_OBJECT, format!("invalid parameter: expected an object ({what})"))
}

/// The error the reference server answers for a stored user or role it would not use.
pub fn stored_error(e: teseladb_auth::users::BadDocument) -> CommandError {
    let code = crate::error::code(e.code).unwrap_or(crate::error::INTERNAL_ERROR);
    CommandError::new(code, e.message)
}

/// The roles of a stored user or role: `{role, db}` or a string each.
fn check_roles(d: &Document) -> Result<(), CommandError> {
    let roles = match d.get("roles") {
        Some(Value::Array(a)) => a,
        Some(_) => return Err(expected_object("roles")),
        None => return Err(expected_object("")),
    };
    for r in roles {
        match r {
            // A role named by a string is stored as given (measured: `roles: ["read"]`).
            Value::String(_) => {}
            Value::Document(r) => {
                text_field(r, "role")?;
                text_field(r, DB)?;
            }
            _ => {
                let msg = "User and role names must be either strings or objects";
                return Err(CommandError::new(BAD_VALUE, msg));
            }
        }
    }
    Ok(())
}

fn user_entry(d: &Document) -> Result<Entry, CommandError> {
    let name = text_field(d, "user")?;
    let db = text_field(d, DB)?;
    match d.get("credentials") {
        Some(Value::Document(_)) => {}
        Some(_) => return Err(expected_object("credentials")),
        None => return Err(expected_object("")),
    }
    check_roles(d)?;
    // The reference server stores the document as it is in the temporary collection (measured: no
    // `userId` added, roles as given).
    Ok(Entry { db, name, doc: d.to_bytes()? })
}

fn role_entry(d: &Document) -> Result<Entry, CommandError> {
    let name = text_field(d, "role")?;
    let db = text_field(d, DB)?;
    let privileges = match d.get("privileges") {
        Some(Value::Array(a)) => a,
        Some(_) => return Err(expected_object("privileges")),
        None => return Err(expected_object("")),
    };
    check_roles(d)?;
    let unknown: Vec<&str> = privileges
        .iter()
        .filter_map(|p| match p {
            Value::Document(p) => match p.get("actions") {
                Some(Value::Array(a)) => Some(a),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .filter_map(|a| match a {
            Value::String(n) if ActionType::from_name(n).is_none() => Some(n.as_str()),
            _ => None,
        })
        .collect();
    if !unknown.is_empty() {
        let msg = format!("Unrecognized action privilege strings: {}", unknown.join(","));
        return Err(CommandError::new(FAILED_TO_PARSE, msg));
    }
    for p in privileges {
        Privilege::from_value(p, ADMIN_DB).map_err(|e| {
            let msg = match e {
                PrivilegeError::Missing(path) => format!("Missing expected field \"{path}\""),
                PrivilegeError::WrongType(path, v, t) => {
                    format!("\"{path}\" had the wrong type. Expected {t}, found {}", type_name(&v))
                }
                PrivilegeError::Bad(m) | PrivilegeError::OutsideDatabase(m) => m,
            };
            CommandError::new(FAILED_TO_PARSE, msg)
        })?;
    }
    Ok(Entry { db, name, doc: d.to_bytes()? })
}

/// The changes that make the users or roles of Group 0 match `temp`, appended to `ops`.
fn plan<B: Backend + ?Sized>(
    backend: &mut B,
    kind: Principal,
    temp: &[Entry],
    a: &MergeArgs,
    ops: &mut Vec<SecurityOp>,
) -> Result<(), CommandError> {
    let scope = if a.db.is_empty() { None } else { Some(a.db.as_str()) };
    let name_field = kind.name_field();
    // The stored documents in scope, with their name and database when they are strings.
    let mut existing: Vec<(Option<(String, String)>, Vec<u8>)> = Vec::new();
    for bytes in backend.principals(kind, scope)? {
        let d = Document::from(RawDoc::from_bytes(&bytes)?);
        let named = text_field(&d, DB).ok().zip(text_field(&d, name_field).ok());
        existing.push((named, bytes));
    }
    let keys = super::authz_writes::key_error;
    let is = |n: &Option<(String, String)>, e: &Entry| {
        n.as_ref().is_some_and(|(d, x)| d == &e.db && x == &e.name)
    };
    if a.drop {
        for (named, old) in &existing {
            if !temp.iter().any(|e| is(named, e)) {
                ops.push(SecurityOp::remove(kind, old.clone()).map_err(keys)?);
            }
        }
    }
    for e in temp {
        let old = existing.iter().find(|(n, _)| is(n, e)).map(|x| x.1.clone());
        match old {
            None => ops.push(SecurityOp::put(kind, Expect::Absent, e.doc.clone()).map_err(keys)?),
            Some(old) if a.drop && old != e.doc => {
                let op = SecurityOp::put(kind, Expect::Exactly(old.clone()), e.doc.clone())
                    .map_err(keys)?;
                if op.id == id_key(&old).map_err(keys)? {
                    ops.push(op);
                } else {
                    // Another `_id`: the old document goes and the new one is added.
                    ops.push(SecurityOp::remove(kind, old).map_err(keys)?);
                    ops.push(SecurityOp::put(kind, Expect::Absent, e.doc.clone()).map_err(keys)?);
                }
            }
            Some(_) => {}
        }
    }
    Ok(())
}
