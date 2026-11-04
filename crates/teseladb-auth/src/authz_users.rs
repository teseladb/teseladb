//! What the user and role commands need (013 FR-011), measured on RefDb 9.0.4: their own
//! action on the database, `grantRole` on the database of each role and privilege they grant,
//! `revokeRole` on that of each one they take away, and the cases where a user may act on itself.

use teseladb_bson::{Document, Value};

use crate::actions::ActionType as A;
use crate::authz::{Check, Need, Requirement, Target, needs};
use crate::authz_rules::Subject;
use crate::roles::ADMIN_DB;
use crate::users::{RoleName, UserName};

fn first(cmd: &Document) -> &Value {
    cmd.iter().next().map_or(&Value::Null, |(_, v)| v)
}

/// The roles of field `f`: names on `db` or `{role, db}`.
fn roles(cmd: &Document, f: &str, db: &str) -> Vec<RoleName> {
    let Some(Value::Array(a)) = cmd.get(f) else { return Vec::new() };
    a.iter()
        .filter_map(|v| match v {
            Value::String(r) => Some(RoleName::new(r, db)),
            other => RoleName::from_value(other),
        })
        .collect()
}

/// The database whose `grantRole` (or `revokeRole`) a privilege needs: its resource's database,
/// `admin` for the cluster, any resource and every database.
fn privilege_dbs(cmd: &Document) -> Vec<String> {
    let Some(Value::Array(a)) = cmd.get("privileges") else { return Vec::new() };
    let mut out: Vec<String> = a
        .iter()
        .filter_map(|p| match p {
            Value::Document(p) => match p.get("resource") {
                Some(Value::Document(r)) => Some(match r.get("db") {
                    Some(Value::String(d)) if !d.is_empty() => d.clone(),
                    _ => ADMIN_DB.to_owned(),
                }),
                _ => None,
            },
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn on_role_dbs(names: &[RoleName], action: A) -> Vec<Need> {
    names.iter().map(|r| Need::new(Target::database(&r.db), action)).collect()
}

fn needs_all(mut list: Vec<Need>) -> Check {
    list.sort();
    list.dedup();
    Check::Needs(vec![Requirement { options: vec![list] }])
}

/// The user or role names of a `usersInfo` or `rolesInfo` value: `None` for the forms that list
/// a whole database or every database.
fn names(v: &Value, db: &str, key: &str) -> Option<Vec<(String, String)>> {
    let one = |v: &Value| match v {
        Value::String(n) => Some((n.clone(), db.to_owned())),
        Value::Document(d) => match (d.get(key), d.get("db")) {
            (Some(Value::String(n)), Some(Value::String(d))) => Some((n.clone(), d.clone())),
            _ => None,
        },
        _ => None,
    };
    match v {
        Value::Array(a) => a.iter().map(one).collect(),
        Value::String(_) => one(v).map(|n| vec![n]),
        Value::Document(d) if d.get(key).is_some() => one(v).map(|n| vec![n]),
        _ => None,
    }
}

fn is_self(who: &Subject<'_>, name: &str, db: &str) -> bool {
    who.user == &UserName::new(name, db)
}

/// `createUser` and `createRole`: `own` on the database, `grantRole` for what they grant.
pub(crate) fn create(db: &str, cmd: &Document, own: A) -> Check {
    let here = Target::database(db);
    let mut list = vec![Need::new(here.clone(), own)];
    list.extend(grants(db, cmd));
    if cmd.get("authenticationRestrictions").is_some() {
        list.push(Need::new(here, A::SetAuthenticationRestriction));
    }
    needs_all(list)
}

/// `grantRole` on the database of each role and privilege of the command.
fn grants(db: &str, cmd: &Document) -> Vec<Need> {
    let mut list = on_role_dbs(&roles(cmd, "roles", db), A::GrantRole);
    list.extend(privilege_dbs(cmd).iter().map(|d| Need::new(Target::database(d), A::GrantRole)));
    list
}

/// `updateRole`: `revokeRole` on every database, and what [`create`] asks for the rest.
pub(crate) fn update_role(db: &str, cmd: &Document) -> Check {
    let mut list = vec![Need::new(Target::AnyNormal, A::RevokeRole)];
    list.extend(grants(db, cmd));
    if cmd.get("authenticationRestrictions").is_some() {
        list.push(Need::new(Target::database(db), A::SetAuthenticationRestriction));
    }
    needs_all(list)
}

/// The commands that grant or revoke roles: `action` on the database of each role.
pub(crate) fn on_roles(db: &str, cmd: &Document, action: A) -> Check {
    needs_all(on_role_dbs(&roles(cmd, "roles", db), action))
}

/// The commands that grant or revoke privileges: `action` on each privilege's database.
pub(crate) fn on_privileges(cmd: &Document, action: A) -> Check {
    needs_all(privilege_dbs(cmd).iter().map(|d| Need::new(Target::database(d), action)).collect())
}

/// `usersInfo`; with `authenticatedMechanism` also `internal` on the cluster (013 FR-033: a
/// probe of minimal privileges on RefDb Enterprise 9.0.4 found `viewUser` and `internal`). A
/// value that names no mechanism needs nothing more: the command refuses it first (measured on a
/// `refrouter` 9.0.4 with `root`, which has no `internal`).
pub(crate) fn users_info(db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    let check = info(true, db, cmd, who);
    let names_a_mechanism = matches!(
        cmd.get("authenticatedMechanism"),
        Some(Value::String(m)) if crate::sasl::SaslMechanism::from_name(m).is_some()
    );
    if !names_a_mechanism {
        return check;
    }
    let internal = Requirement::all(&Target::Cluster, &[A::Internal]);
    match check {
        Check::Needs(mut reqs) => {
            reqs.push(internal);
            Check::Needs(reqs)
        }
        Check::Open => Check::Needs(vec![internal]),
        other => other,
    }
}

/// `rolesInfo`.
pub(crate) fn roles_info(db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    info(false, db, cmd, who)
}

/// `updateUser`.
pub(crate) fn update_user(db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    let here = Target::database(db);
    let me = matches!(first(cmd), Value::String(n) if is_self(who, n, db));
    let mut reqs = Vec::new();
    let mut own = |field: &str, other: A, mine: A| {
        if cmd.get(field).is_none() {
            return;
        }
        reqs.push(if me {
            Requirement::either(needs(&here, &[other]), needs(&here, &[mine]))
        } else {
            Requirement::all(&here, &[other])
        });
    };
    own("pwd", A::ChangePassword, A::ChangeOwnPassword);
    own("customData", A::ChangeCustomData, A::ChangeOwnCustomData);
    if cmd.get("roles").is_some() {
        reqs.push(Requirement::all(&Target::AnyNormal, &[A::GrantRole, A::RevokeRole]));
    }
    if cmd.get("authenticationRestrictions").is_some() {
        reqs.push(Requirement::all(&here, &[A::SetAuthenticationRestriction]));
    }
    Check::Needs(reqs)
}

fn info(users: bool, db: &str, cmd: &Document, who: &Subject<'_>) -> Check {
    let (key, action) = if users { ("user", A::ViewUser) } else { ("role", A::ViewRole) };
    let v = first(cmd);
    if users && matches!(v, Value::Document(d) if d.get("forAllDBs").is_some()) {
        return Check::Needs(vec![Requirement::all(&Target::Cluster, &[action])]);
    }
    let Some(list) = names(v, db, key) else {
        return Check::Needs(vec![Requirement::all(&Target::database(db), &[action])]);
    };
    let own = |(n, d): &(String, String)| {
        if users { is_self(who, n, d) } else { who.roles.contains(&RoleName::new(n, d)) }
    };
    if !list.is_empty() && list.iter().all(own) {
        return Check::Open;
    }
    needs_all(list.iter().map(|(_, d)| Need::new(Target::database(d), action)).collect())
}

/// `_mergeAuthzCollections` (013 FR-030), as a probe of minimal privileges found on a `refrouter`
/// 9.0.4: `createUser`, `createRole`, `grantRole` and `revokeRole` on every database, with
/// `drop` also `dropUser` and `dropRole`, and `find` on each temporary collection it names.
pub fn merge_authz_collections(cmd: &Document) -> Check {
    let mut actions = vec![A::CreateUser, A::CreateRole, A::GrantRole, A::RevokeRole];
    if cmd.get("drop") == Some(&Value::Boolean(true)) {
        actions.extend([A::DropUser, A::DropRole]);
    }
    let mut list = needs(&Target::AnyNormal, &actions);
    for f in ["tempUsersCollection", "tempRolesCollection"] {
        if let Some(Value::String(ns)) = cmd.get(f)
            && let Some(t) = Target::namespace(ns)
        {
            list.extend(needs(&t, &[A::Find]));
        }
    }
    needs_all(list)
}
