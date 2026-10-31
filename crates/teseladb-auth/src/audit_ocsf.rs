//! The audit log in the Open Cybersecurity Schema Framework (`auditLog.schema: OCSF`, 013
//! FR-031), with the classes, activities and fields RefDb Enterprise 9.0.4 writes for each
//! action type (measured on 31-10-2026): the message of [`crate::audit::Event`] rebuilt from its
//! parameters, which keep RefDb's own schema.

use teseladb_bson::{Document, Value, extjson};

use crate::audit::{AuditType, Client, Endpoint, Event, ProcessInfo};

/// `metadata.product`.
const PRODUCT: &str = "RefDb Server";
/// `metadata.version`.
const VERSION: &str = "1.0.0";
/// `severity_id` of every message TeselaDB writes (informational).
const SEVERITY_INFORMATIONAL: i32 = 1;
/// `type_id` of a user: a client, or the server itself.
const USER_REGULAR: i32 = 1;
const USER_SYSTEM: i32 = 3;
/// `activity_id` values.
const CREATE: i32 = 1;
const READ: i32 = 2;
const UPDATE: i32 = 3;
const DELETE: i32 = 4;
const ASSIGN: i32 = 7;
const REVOKE: i32 = 8;
const REMOVE: i32 = 6;
const OTHER: i32 = 99;
const UNKNOWN: i32 = 0;

/// A class with its category.
#[derive(Clone, Copy)]
struct Class {
    category: i32,
    uid: i32,
}

const PROCESS_ACTIVITY: Class = Class { category: 1, uid: 1007 };
const ACCOUNT_CHANGE: Class = Class { category: 3, uid: 3001 };
const AUTHENTICATION: Class = Class { category: 3, uid: 3002 };
const ENTITY_MANAGEMENT: Class = Class { category: 3, uid: 3004 };
const NETWORK_ACTIVITY: Class = Class { category: 4, uid: 4001 };
const DEVICE_CONFIG_STATE: Class = Class { category: 5, uid: 5002 };
const API_ACTIVITY: Class = Class { category: 6, uid: 6003 };

/// Which ends of the connection a message names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ends {
    None,
    /// `src_endpoint` and `direct_endpoint`.
    Source,
    /// `dst_endpoint`, `src_endpoint` and `direct_endpoint`.
    All,
}

fn text(p: &Document, key: &str) -> String {
    match p.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn endpoint(e: &Endpoint) -> Option<Document> {
    match e {
        Endpoint::Address { ip, port } => {
            Some(Document::new().with("ip", ip.as_str()).with("port", i32::from(*port)))
        }
        Endpoint::Unix(path) => Some(Document::new().with("path", path.as_str())),
        Endpoint::SystemUser => None,
    }
}

/// `db.name` names of a list of `{role|user, db}` documents.
fn names(v: Option<&Value>, key: &str) -> Vec<Value> {
    let Some(Value::Array(items)) = v else { return Vec::new() };
    items
        .iter()
        .filter_map(|i| match i {
            Value::Document(d) => Some(Value::from(format!("{}.{}", text(d, "db"), text(d, key)))),
            _ => None,
        })
        .collect()
}

fn groups(roles: Vec<Value>) -> Value {
    Value::Array(
        roles.into_iter().map(|r| Value::Document(Document::new().with("name", r))).collect(),
    )
}

/// The actor: a client (with its user and roles once logged in) or the server.
fn actor(c: &Client) -> Document {
    if c.remote == Endpoint::SystemUser {
        return Document::new().with("type_id", USER_SYSTEM);
    }
    let mut user = Document::new().with("type_id", USER_REGULAR);
    if let Some(u) = c.users.first() {
        user.push("name", format!("{}.{}", u.db, u.user));
        let roles: Vec<Value> =
            c.roles.iter().map(|r| Value::from(format!("{}.{}", r.db, r.role))).collect();
        user.push("groups", groups(roles));
    }
    user
}

/// The message's own fields after the common ones, and which ends it names.
struct Body {
    class: Class,
    activity: i32,
    ends: Ends,
    fields: Document,
}

fn body(class: Class, activity: i32, ends: Ends, fields: Document) -> Body {
    Body { class, activity, ends, fields }
}

fn unmapped(atype: AuditType, p: &Document, keys: &[&str]) -> Document {
    let mut d = Document::new().with("atype", atype.name());
    for k in keys {
        if let Some(v) = p.get(k) {
            d.push(*k, v.clone());
        }
    }
    d
}

/// The target user of a user command: its name and, with `roles`, the roles it ends with.
fn target_user(p: &Document, with_roles: bool) -> Document {
    let mut u = Document::new()
        .with("type_id", USER_REGULAR)
        .with("name", format!("{}.{}", text(p, "db"), text(p, "user")));
    let roles = names(p.get("roles"), "role");
    if with_roles && !roles.is_empty() {
        u.push("groups", groups(roles));
    }
    u
}

fn role_unmapped(atype: AuditType, p: &Document, keys: &[&str]) -> Document {
    let mut d = Document::new()
        .with("atype", atype.name())
        .with("role", format!("{}.{}", text(p, "db"), text(p, "role")));
    for k in keys {
        match (*k, p.get(k)) {
            ("roles", Some(v)) => d.push("roles", Value::Array(names(Some(v), "role"))),
            (_, Some(v)) => d.push(*k, v.clone()),
            _ => {}
        }
    }
    d
}

fn entity(name: String, kind: &str) -> Document {
    Document::new().with("name", name).with("type", kind)
}

/// The activity of a command's authorization check: the CRUD verbs, and 0 for the rest.
fn command_activity(command: &str) -> i32 {
    match command {
        "insert" => CREATE,
        "find" => READ,
        "update" => UPDATE,
        "delete" => DELETE,
        _ => UNKNOWN,
    }
}

fn os(process: Option<&ProcessInfo>) -> Document {
    let (name, version) = process
        .map_or((String::new(), String::new()), |p| (p.os_name.clone(), p.os_version.clone()));
    Document::new().with("name", name).with("version", version).with("type_id", 0)
}

fn body_of(event: &Event, process: Option<&ProcessInfo>) -> Body {
    use AuditType as A;
    let p = &event.param;
    let ns = || text(p, "ns");
    match event.atype {
        A::AuthCheck => {
            let response = match event.result {
                0 => Document::new().with("code", 0),
                13 => Document::new().with("code", 13).with("error", "Unauthorized"),
                code => Document::new().with("code", code).with("error", format!("Location{code}")),
            };
            let api = Document::new()
                .with("operation", text(p, "command"))
                .with("request", Document::new().with("uid", ns()))
                .with("response", response);
            let mut fields = Document::new().with("api", api);
            if let Some(args) = p.get("args") {
                fields.push("unmapped", Document::new().with("args", args.clone()));
            }
            body(API_ACTIVITY, command_activity(&text(p, "command")), Ends::All, fields)
        }
        A::Authenticate => {
            let user = Document::new()
                .with("type_id", USER_REGULAR)
                .with("name", format!("{}.{}", text(p, "db"), text(p, "user")));
            let fields = Document::new()
                .with("user", user)
                .with("auth_protocol", text(p, "mechanism"))
                .with("unmapped", Document::new().with("atype", event.atype.name()));
            body(AUTHENTICATION, CREATE, Ends::All, fields)
        }
        A::Logout => {
            let fields = Document::new()
                .with("user", Document::new().with("type_id", USER_SYSTEM))
                .with("message", format!("Reason: {}", text(p, "reason")));
            body(AUTHENTICATION, 2, Ends::None, fields)
        }
        A::ClientMetadata => {
            let meta = match p.get("clientMetadata") {
                Some(Value::Document(d)) => d.clone(),
                _ => Document::new(),
            };
            body(NETWORK_ACTIVITY, OTHER, Ends::All, Document::new().with("unmapped", meta))
        }
        A::CreateUser | A::UpdateUser => {
            let activity = if event.atype == A::CreateUser { CREATE } else { UPDATE };
            let mut u = unmapped(event.atype, p, &["customData"]);
            u.push("authenticationRestrictions", Value::Array(Vec::new()));
            let fields = Document::new().with("user", target_user(p, true)).with("unmapped", u);
            body(ACCOUNT_CHANGE, activity, Ends::Source, fields)
        }
        A::DropUser => body(
            ACCOUNT_CHANGE,
            REMOVE,
            Ends::Source,
            Document::new().with("user", target_user(p, false)),
        ),
        A::GrantRolesToUser | A::RevokeRolesFromUser => {
            let activity = if event.atype == A::GrantRolesToUser { ASSIGN } else { REVOKE };
            let u = Document::new()
                .with("atype", event.atype.name())
                .with("roles", Value::Array(names(p.get("roles"), "role")));
            // The target user goes before the ends (measured).
            let mut b = body(ACCOUNT_CHANGE, activity, Ends::Source, Document::new());
            b.fields = Document::new().with("user", target_user(p, false)).with("unmapped", u);
            b
        }
        A::CreateRole => {
            let u = role_unmapped(event.atype, p, &["roles", "privileges"]);
            body(ACCOUNT_CHANGE, CREATE, Ends::Source, Document::new().with("unmapped", u))
        }
        A::UpdateRole => {
            let u = role_unmapped(event.atype, p, &["roles", "privileges"]);
            body(ACCOUNT_CHANGE, OTHER, Ends::Source, Document::new().with("unmapped", u))
        }
        A::DropRole => {
            let u = role_unmapped(event.atype, p, &[]);
            body(ACCOUNT_CHANGE, REMOVE, Ends::Source, Document::new().with("unmapped", u))
        }
        A::GrantRolesToRole | A::RevokeRolesFromRole => {
            let activity = if event.atype == A::GrantRolesToRole { ASSIGN } else { REVOKE };
            let u = role_unmapped(event.atype, p, &["roles"]);
            body(ACCOUNT_CHANGE, activity, Ends::Source, Document::new().with("unmapped", u))
        }
        A::GrantPrivilegesToRole | A::RevokePrivilegesFromRole => {
            let activity = if event.atype == A::GrantPrivilegesToRole { ASSIGN } else { REVOKE };
            let u = role_unmapped(event.atype, p, &["privileges"]);
            body(ACCOUNT_CHANGE, activity, Ends::Source, Document::new().with("unmapped", u))
        }
        A::DropAllUsersFromDatabase => {
            let u = Document::new()
                .with("atype", event.atype.name())
                .with("allUsersFromDatabase", text(p, "db"));
            body(ACCOUNT_CHANGE, REMOVE, Ends::Source, Document::new().with("unmapped", u))
        }
        A::DropAllRolesFromDatabase => {
            let u = Document::new().with("atype", event.atype.name()).with("db", text(p, "db"));
            body(ACCOUNT_CHANGE, REMOVE, Ends::Source, Document::new().with("unmapped", u))
        }
        A::CreateDatabase | A::CreateCollection | A::DropCollection | A::DropDatabase => {
            let (activity, kind) = match event.atype {
                A::CreateDatabase => (CREATE, "create_database"),
                A::CreateCollection => (CREATE, "create_collection"),
                A::DropCollection => (DELETE, "drop_collection"),
                _ => (DELETE, "drop_database"),
            };
            let fields = Document::new().with("entity", entity(ns(), kind));
            body(ENTITY_MANAGEMENT, activity, Ends::Source, fields)
        }
        A::CreateIndex => {
            let name = format!("{}@{}", text(p, "indexName"), ns());
            let mut e = entity(name, "create_index");
            e.push("data", p.get("indexSpec").cloned().unwrap_or(Value::Null));
            let fields = Document::new()
                .with("entity", e)
                .with("comment", format!("IndexBuildState: {}", text(p, "indexBuildState")));
            body(ENTITY_MANAGEMENT, CREATE, Ends::Source, fields)
        }
        A::DropIndex => {
            let name = format!("{}@{}", text(p, "indexName"), ns());
            let fields = Document::new().with("entity", entity(name, "drop_index"));
            body(ENTITY_MANAGEMENT, DELETE, Ends::Source, fields)
        }
        A::RenameCollection => {
            let fields = Document::new()
                .with("entity", entity(text(p, "old"), "rename_collection_source"))
                .with("entity_result", entity(text(p, "new"), "rename_collection_destination"));
            body(ENTITY_MANAGEMENT, UPDATE, Ends::Source, fields)
        }
        A::Startup => {
            let mut fields = Document::new();
            if let Some(pr) = process {
                fields.push("pid", i64::from(pr.pid));
            }
            fields.push("type_id", 0);
            fields.push("os", os(process));
            fields.push("status_id", 1);
            let mut u = Document::new().with("atype", event.atype.name());
            if let Some(o) = p.get("startupOptions") {
                u.push("startup_options", o.clone());
            }
            fields.push("unmapped", u);
            body(PROCESS_ACTIVITY, CREATE, Ends::None, fields)
        }
        A::Shutdown => body(
            PROCESS_ACTIVITY,
            2,
            Ends::None,
            Document::new().with("type_id", 0).with("os", os(process)),
        ),
        A::ApplicationMessage => {
            let u = unmapped(event.atype, p, &["msg"]);
            body(PROCESS_ACTIVITY, OTHER, Ends::None, Document::new().with("unmapped", u))
        }
        A::GetClusterParameter => {
            let u = p.clone();
            body(API_ACTIVITY, READ, Ends::All, Document::new().with("unmapped", u))
        }
        A::SetClusterParameter | A::UpdateCachedClusterServerParameter | A::AuditConfigure => {
            let mut u = Document::new().with("atype", event.atype.name());
            for (k, v) in p.iter() {
                u.push(k, v.clone());
            }
            body(DEVICE_CONFIG_STATE, CREATE, Ends::None, Document::new().with("unmapped", u))
        }
        other => {
            let mut u = Document::new().with("atype", other.name());
            for (k, v) in p.iter() {
                u.push(k, v.clone());
            }
            body(PROCESS_ACTIVITY, OTHER, Ends::None, Document::new().with("unmapped", u))
        }
    }
}

/// The OCSF message of `event` of `client`.
pub fn document(event: &Event, client: &Client, process: Option<&ProcessInfo>) -> Document {
    let b = body_of(event, process);
    let metadata = Document::new()
        .with("correlation_uid", extjson::uuid_text(&client.uuid))
        .with("product", PRODUCT)
        .with("version", VERSION);
    // The logout of a closed connection is the server's (measured).
    let actor_user = match event.atype {
        AuditType::Logout => Document::new().with("type_id", USER_SYSTEM),
        _ => actor(client),
    };
    let mut d = Document::new()
        .with("activity_id", b.activity)
        .with("category_uid", b.class.category)
        .with("class_uid", b.class.uid)
        .with("time", event.ts_ms)
        .with("severity_id", SEVERITY_INFORMATIONAL)
        .with("type_uid", i64::from(b.class.uid) * 100 + i64::from(b.activity))
        .with("metadata", metadata)
        .with("actor", Document::new().with("user", actor_user));
    let mut fields = b.fields;
    // A user command's target user goes before the ends, the other fields after.
    let lead = match event.atype {
        AuditType::GrantRolesToUser | AuditType::RevokeRolesFromUser => fields.remove("user"),
        _ => None,
    };
    if let Some(u) = lead {
        d.push("user", u);
    }
    if b.ends == Ends::All
        && let Some(e) = endpoint(&client.local)
    {
        d.push("dst_endpoint", e);
    }
    if b.ends != Ends::None
        && let Some(e) = endpoint(&client.remote)
    {
        d.push("src_endpoint", e.clone());
        d.push("direct_endpoint", e);
    }
    for (k, v) in fields.iter() {
        d.push(k, v.clone());
    }
    d
}
